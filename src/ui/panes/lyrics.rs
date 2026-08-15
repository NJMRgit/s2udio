use std::time::Duration;

use anyhow::Result;
use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{List, ListItem, ListState, Paragraph, StatefulWidget},
};

use super::Pane;
use crate::{
    config::keys::CommonAction,
    core::command::{create_env, run_external_blocking},
    ctx::Ctx,
    mpd::commands::{Song, State},
    shared::{
        events::WorkRequest,
        ext::duration::DurationExt,
        keys::ActionEvent,
        lrc::{Lrc, LrcEditSession, get_lrc_path},
        macros::{modal, status_error, status_info},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_query::run_status_update,
    },
    ui::{
        UiEvent,
        modals::input_modal::InputModal,
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
            word_areas.push(WordArea {
                rect: Rect { x, y: row.y, width: word_w + 2 + time_w, height: 1 },
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
fn plain_edit_chunks(line: &crate::shared::lrc::EditableLine, timestamp: bool, width: usize) -> Vec<String> {
    let formatted = if timestamp && !line.content.is_empty() {
        format!("[{}] {}", LrcEditSession::format_time(line.time), line.content)
    } else {
        line.content.clone()
    };
    textwrap::wrap(&formatted, width).into_iter().map(|s| s.as_ref().to_owned()).collect()
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
            .is_some_and(|(_, song)| self.wrong_song_file.as_deref() == Some(song.file.as_str()))
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

        // The frame needs at least four rows: blank + body + margin +
        // buttons. (The render entry already gates the whole pane below
        // `MIN_PANE_CONTENT_HEIGHT`; this is a defensive guard.)
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

        // A single margin line — a full-width quiet `─` rule in the border
        // style — separates the body from the button row at the bottom.
        // The top of the frame is a blank row: the lyrics start one row
        // below the pane's top border.
        let margin_style = ctx.config.as_border_style();
        let width = area.width as usize;
        let bottom_margin_y = area.bottom().saturating_sub(2);
        let buttons_y = area.bottom().saturating_sub(1);
        buf.set_stringn(area.x, bottom_margin_y, "─".repeat(width), width, margin_style);

        let mouse = ctx.mouse_pos();
        let hovered_style = ctx.config.theme.hovered_item_style;

        // Round 34: the edit-mode pencil button, left-aligned on the same
        // row (one cell in from the left border), independent of the
        // right-aligned cluster's fit-collapse logic. Icon only — no
        // text: `✎` off, `✏` while edit mode is ON (persistent active
        // state), `⭘` while physically held (the pressed marker).
        let edit_glyph = if self.pressed_btn == Some(LyricsBtn::Edit) {
            "⭘"
        } else if self.edit_mode {
            "✏"
        } else {
            "✎"
        };
        self.edit_btn_area = Rect { x: area.x + 1, y: buttons_y, width: 1, height: 1 };
        let edit_hovered = mouse.is_some_and(|p| self.edit_btn_area.contains(p));
        buf.set_stringn(
            area.x + 1,
            buttons_y,
            edit_glyph,
            1,
            if edit_hovered { crate::config::hover_style(base).patch(hovered_style) } else { base },
        );

        // Right-aligned button cluster: `● hide lyrics | ● fetch lyrics`
        // (or `● show lyrics` while the current song is wrong-marked),
        // collapsed to `● hide | ● fetch` / `● show | ● fetch` when
        // narrow. The `|` separator is space-padded on both sides and
        // never hover-highlighted. The cluster sits one cell in from the
        // right border.
        let glyph_of = |pressed: bool| if pressed { "⭘" } else { "●" };
        let hide_show = if self.is_wrong(ctx) { "show" } else { "hide" };
        let full_wrong = format!("{} {hide_show} lyrics", glyph_of(self.pressed_btn == Some(LyricsBtn::Wrong)));
        let full_fetch = format!("{} fetch lyrics", glyph_of(self.pressed_btn == Some(LyricsBtn::Fetch)));
        let short_wrong = format!("{} {hide_show}", glyph_of(self.pressed_btn == Some(LyricsBtn::Wrong)));
        let short_fetch = format!("{} fetch", glyph_of(self.pressed_btn == Some(LyricsBtn::Fetch)));

        // Pick the longest form that fits (full > short > hidden). The
        // cluster is `wrong | fetch` — the separator adds 3 cells.
        let (wrong_label, fetch_label) =
            if full_wrong.width() + 3 + full_fetch.width() <= area.width as usize {
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
        // One-cell right margin (round 12): the cluster sits one cell in
        // from the right border, `start = width - cluster_w - 1`.
        let start = area.x + area.width.saturating_sub(cluster_w).saturating_sub(1);
        self.wrong_btn_area = Rect { x: start, y: buttons_y, width: wrong_w, height: 1 };
        self.fetch_btn_area =
            Rect { x: start + wrong_w + 3, y: buttons_y, width: fetch_w, height: 1 };

        let wrong_hovered = mouse.is_some_and(|p| self.wrong_btn_area.contains(p));
        let fetch_hovered = mouse.is_some_and(|p| self.fetch_btn_area.contains(p));

        // Hover = the queue-list row highlight (bg + bold) plus the
        // standard label-text brightening, applied to the **label text
        // only** (round 12): the marker glyph keeps its completely normal
        // style — no hover background, no bold, no brightening.
        fn button_line(
            label: &str,
            hovered: bool,
            base: Style,
            hovered_style: Style,
        ) -> Line<'_> {
            // The separator space is part of the glyph span (completely
            // normal style): only the text itself — `hide lyrics` /
            // `fetch lyrics` — gets the hover treatment (round 12
            // follow-up: the highlight must not cover the leading space).
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
            Line::from(vec![
                Span::styled(glyph, glyph_style),
                Span::styled(text, text_style),
            ])
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
        // The separator between the two buttons (space-padded `|`), never
        // hover-highlighted.
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
            emulator,
            crate::shared::terminal::Emulator::Kitty
                | crate::shared::terminal::Emulator::Ghostty
                | crate::shared::terminal::Emulator::WezTerm
                | crate::shared::terminal::Emulator::Konsole
                | crate::shared::terminal::Emulator::Foot
                | crate::shared::terminal::Emulator::Iterm2
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
        ctx.scheduler.schedule_replace(id, std::time::Duration::from_millis(300), move |(tx, _)| {
            Ok(tx.send(crate::shared::events::AppEvent::UiEvent(
                crate::ui::UiAppEvent::LyricsReleaseCheck,
            ))?)
        });
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
            let env_refs =
                envs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect::<Vec<_>>();
            if let Err(err) = run_external_blocking(&command, env_refs) {
                status_error!("Failed to fetch lyrics: '{err}'");
                return;
            }
            // Re-index the expected .lrc so the pane reloads the new
            // lyrics (LyricsIndexed -> update_lyrics).
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
        if self.edit_mode
            && matches!(ctx.status.state, State::Pause)
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
                    .min_by(|a, b| a.1.time.abs_diff(elapsed).cmp(&b.1.time.abs_diff(elapsed)))
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
        let default_style = Style::default().fg(ctx.config.theme.text_color.unwrap_or_default());
        let selected_style = ctx.config.theme.highlighted_item_style;
        let timing_style = ctx
            .config
            .theme
            .lyrics
            .edit_timing
            .unwrap_or_else(|| default_style.add_modifier(Modifier::DIM));

        let anchor = self.anchor_line(ctx);
        let areas = Layout::vertical((0..area.height).map(|_| Constraint::Length(1))).split(area);
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
                            EditUnit { word: w.text.clone(), time, word_w: w.text.width(), time_w }
                        })
                        .collect()
                })
                .unwrap_or_default()
        };

        // The anchor line: rows centered around the middle row.
        let current_units = units_of(anchor);
        let current_rows = wrap_edit_units(&current_units, width);
        let active_start = middle_row.saturating_sub(current_rows.len().saturating_sub(1) as u16);
        for (ri, row_units) in current_rows.iter().enumerate() {
            let Some(row) = areas.get((active_start + ri as u16) as usize).copied() else {
                break;
            };
            drawer.place(row, &current_units, row_units, anchor, &mut word_areas);
        }
        let mut after_row = active_start + current_rows.len() as u16;

        // Lines before the anchor fill upward.
        let mut before_row = active_start;
        let mut before_line = anchor;
        while before_line > 0 && before_row > 0 {
            before_line -= 1;
            let units = units_of(before_line);
            if units.is_empty() {
                for chunk in plain_edit_chunks(&session.lines[before_line], timestamp, width)
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

        // Lines after the anchor fill downward.
        let mut after_line = anchor;
        while after_line + 1 < session.lines.len() && after_row < areas.len() as u16 {
            after_line += 1;
            let units = units_of(after_line);
            if units.is_empty() {
                for chunk in plain_edit_chunks(&session.lines[after_line], timestamp, width) {
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
            // Reload so the karaoke view reflects the saved timings.
            if let Err(err) = self.update_lyrics(ctx) {
                status_error!("Failed to reload lyrics file: '{err}'");
            }
            ctx.render()?;
        }
        // Round 35: the cava pane swaps the visualizer for the edit-controls
        // legend while edit mode is on.
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
        // Round 35/40: a just-inserted line or word (from the insert
        // modal) is selected on reload so the user lands on the new
        // content. A word target needs the word to exist; a line target
        // (word 0) needs the line to have words.
        if let Some((sel_line, sel_word)) = pending_select
            && self
                .edit_session
                .as_ref()
                .is_some_and(|s| {
                    s.lines.get(sel_line).is_some_and(|ln| {
                        !ln.words.is_empty() && (sel_word == 0 || sel_word < ln.words.len())
                    })
                })
        {
            self.edit_selection = Some((sel_line, sel_word));
            return Ok(());
        }
        let valid = selection.is_some_and(|(l, w)| {
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
        if let Some(session) = &mut self.edit_session
            && session.is_dirty()
        {
            session.save()?;
        }
        modal!(
            ctx,
            InputModal::new(ctx)
                .title("Word time (mm:ss.xx)")
                .confirm_label("Set")
                .input_label("Time:")
                .initial_value(current)
                .on_confirm(move |ctx, value| {
                    let Some(time) = LrcEditSession::parse_time(value) else {
                        status_error!("Invalid time: expected mm:ss.xx");
                        return Ok(());
                    };
                    let raw = std::fs::read_to_string(&path)?;
                    let new_raw = LrcEditSession::apply_to_raw(&raw, line, word, time)?;
                    LrcEditSession::write_atomic(&path, &new_raw)?;
                    crate::shared::macros::try_skip!(
                        ctx.work_sender
                            .send(crate::shared::events::WorkRequest::IndexSingleLrc { path }),
                        "Failed to request lyrics index update"
                    );
                    Ok(())
                })
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
            // The line is gone: land on the next selectable line, else
            // the previous one.
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
        if let Some(session) = &mut self.edit_session
            && session.is_dirty()
        {
            session.save()?;
        }
        modal!(
            ctx,
            InputModal::new(ctx)
                .title("Lyrics text")
                .confirm_label("Set")
                .input_label("Text:")
                .initial_value(current)
                .on_confirm(move |ctx, value| {
                    let raw = std::fs::read_to_string(&path)?;
                    let mut session = LrcEditSession::open(path.clone(), raw);
                    session.set_line_text(line_idx, value)?;
                    session.save()?;
                    crate::shared::macros::try_skip!(
                        ctx.work_sender
                            .send(crate::shared::events::WorkRequest::IndexSingleLrc { path }),
                        "Failed to request lyrics index update"
                    );
                    Ok(())
                })
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
        if let Some(session) = &mut self.edit_session
            && session.is_dirty()
        {
            session.save()?;
        }
        modal!(
            ctx,
            InputModal::new(ctx)
                .title("Line time (mm:ss.xx)")
                .confirm_label("Set")
                .input_label("Time:")
                .initial_value(current)
                .on_confirm(move |ctx, value| {
                    let Some(time) = LrcEditSession::parse_time(value) else {
                        status_error!("Invalid time: expected mm:ss.xx");
                        return Ok(());
                    };
                    let raw = std::fs::read_to_string(&path)?;
                    let mut session = LrcEditSession::open(path.clone(), raw);
                    session.set_line_time(line_idx, time)?;
                    session.save()?;
                    crate::shared::macros::try_skip!(
                        ctx.work_sender
                            .send(crate::shared::events::WorkRequest::IndexSingleLrc { path }),
                        "Failed to request lyrics index update"
                    );
                    Ok(())
                })
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
                    Some(next) if next > anchor.time => anchor.time + (next - anchor.time) / 2,
                    _ => anchor.time + Duration::from_secs(5),
                }
            };
            (session.path().clone(), position, time)
        };
        if let Some(session) = &mut self.edit_session
            && session.is_dirty()
        {
            session.save()?;
        }
        self.pending_insert_select = Some((position, 0));
        modal!(
            ctx,
            InputModal::new(ctx)
                .title(if before { "New lyric before" } else { "New lyric after" })
                .confirm_label("Insert")
                .input_label("Text:")
                .on_confirm(move |ctx, value| {
                    let raw = std::fs::read_to_string(&path)?;
                    let mut session = LrcEditSession::open(path.clone(), raw);
                    let idx = session.insert_line_at(position, time)?;
                    session.set_line_text(idx, value)?;
                    session.save()?;
                    crate::shared::macros::try_skip!(
                        ctx.work_sender
                            .send(crate::shared::events::WorkRequest::IndexSingleLrc { path }),
                        "Failed to request lyrics index update"
                    );
                    Ok(())
                })
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
        if let Some(session) = &mut self.edit_session
            && session.is_dirty()
        {
            session.save()?;
        }
        self.pending_insert_select = Some((line_idx, if before { w } else { w + 1 }));
        modal!(
            ctx,
            InputModal::new(ctx)
                .title(if before { "New word before" } else { "New word after" })
                .confirm_label("Insert")
                .input_label("Word:")
                .on_confirm(move |ctx, value| {
                    let raw = std::fs::read_to_string(&path)?;
                    let mut session = LrcEditSession::open(path.clone(), raw);
                    session.insert_word_at(line_idx, word_idx, !before, value)?;
                    session.save()?;
                    crate::shared::macros::try_skip!(
                        ctx.work_sender
                            .send(crate::shared::events::WorkRequest::IndexSingleLrc { path }),
                        "Failed to request lyrics index update"
                    );
                    Ok(())
                })
        );
        Ok(())
    }

    /// Render the song details (File / Filename / Title / ... with the
    /// yellow group labels), matching the Directories / Radio info boxes.
    /// Scrollable with the mouse wheel; the offset resets when the shown
    /// song changes.
    fn render_info(&mut self, frame: &mut Frame, area: Rect, song: &Song, ctx: &Ctx) {
        let preview = song.to_preview(
            ctx.config.theme.preview_label_style,
            ctx.config.theme.preview_metadata_group_style,
            ctx,
        );
        let mut items: Vec<ListItem> = Vec::new();
        for group in preview {
            if let Some(name) = group.name {
                items.push(ListItem::new(Line::styled(
                    name,
                    group.header_style.unwrap_or_default(),
                )));
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
        StatefulWidget::render(list, list_area, frame.buffer_mut(), &mut self.info_state);

        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self.info_items_len.saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            // NOTE: no `.orientation(...)` here — as_styled_scrollbar already
            // sets it, and ratatui's orientation() resets the symbols to the
            // defaults (▲█║▼), wiping the theme's ↑│↓ style.
            // content_length = max_offset + 1 so the bottom position is
            // reachable (ratatui clamps positions to content_length - 1);
            // the viewport length keeps the thumb proportional to the
            // visible rows.
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
        // White (explicit ANSI white, independent of the theme/blur accent)
        // for the channel name, the subs value and the description body.
        let white = Style::default().fg(Color::White);
        let bold = Style::default().add_modifier(Modifier::BOLD);

        // ---- collect the video's details ----
        // The "year -- " prefix stays fixed while the name marquee-scrolls.
        let mut title_prefix = String::new();
        let mut title = ctx.mpv.title.clone();
        // Second row: "Channel: <name>" (yellow label, white value) on the
        // left, "Subs: 1.71M" right-aligned under the time; or
        // "Episode: <name>   S03E03" for Jellyfin episodes.
        let mut context_left: Vec<Span<'static>> = Vec::new();
        let mut context_right: Vec<Span<'static>> = Vec::new();
        // Scrollable content: the wrapped description only. Each row keeps
        // its link spans so the link under the pointer can lighten.
        let mut body: Vec<InfoBodyLine> = Vec::new();
        // Fixed bottom rows: the credits (never scroll with the description).
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
            // Jellyfin item metadata (or a plain mpv title): the header is
            // "Year -- Series/Movie" (the year prefix fixed, the name
            // scrolling), the episode gets its own row, the overview scrolls
            // and the credits stay pinned below it.
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
                    if let (Some(season), Some(episode)) =
                        (item.season_number, item.index_number)
                    {
                        context_right.push(Span::styled(
                            format!("S{season:02}E{episode:02}"),
                            base,
                        ));
                    }
                }
                if let Some(overview) = item.overview.as_deref().filter(|d| !d.trim().is_empty()) {
                    for line in wrap_to_width(&scrub_emoji(overview), body_width) {
                        push_body(&mut body, &line, list_style);
                    }
                }
                // Credits (Director / Writer / Starring), key-style labels,
                // pinned below the scrolling description.
                if let Some(director) = &item.director {
                    credits.push(Self::credit_line(key_style, white, "Director", director));
                }
                if let Some(writer) = &item.writer {
                    credits.push(Self::credit_line(key_style, white, "Writer", writer));
                }
                if !item.starring.is_empty() {
                    credits.push(Self::credit_line(
                        key_style,
                        white,
                        "Starring",
                        &item.starring.join(", "),
                    ));
                }
            } else if !ctx.mpv.title.is_empty() {
                title = ctx.mpv.title.clone();
            }
        }

        // ---- layout: fixed header, scrollable description, pinned credits ----
        let has_context = !context_left.is_empty() || !context_right.is_empty();
        let header_h = 1 + usize::from(has_context) + usize::from(!body.is_empty());
        let credits_h = credits.len();
        let [header_area, body_area, credits_area] = Layout::vertical([
            Constraint::Length(header_h as u16),
            Constraint::Min(0),
            Constraint::Length(credits_h as u16),
        ])
        .areas(area);
        // The time width is shared by the title row (right-aligned time) and
        // the context row (right-aligned "Subs: …"), so they line up. The
        // duration comes from mpv while a video plays, from the MPD status
        // when a YouTube stream plays as audio.
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
        frame.render_widget(
            Paragraph::new(Span::styled(time_text, bold)).alignment(Alignment::Right),
            time_area,
        );
        // The "year -- " prefix is fixed; only the name marquee-scrolls.
        let prefix_w = title_prefix.chars().count() as u16;
        let (prefix_area, marquee_area) = if prefix_w > 0 && prefix_w < title_area.width {
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
            frame.render_widget(Paragraph::new(Span::styled(title_prefix, base)), prefix_area);
        }
        // The title marquee applies only when the name does not fit the
        // area, and uses the same cycle as the controls-bar carousel: hold
        // 2s at the start, scroll left to the tail, hold 2s at the end,
        // then wrap around (never reversing). The title renders in explicit
        // ANSI white, independent of the auto/blur accent.
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
                frame.render_widget(
                    Paragraph::new(Line::from(context_right)).alignment(Alignment::Right),
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
            // "Description ↴": yellow label, white arrow (list color).
            frame.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::styled("Description", key_style),
                    Span::styled(" ↴", white),
                ])),
                row,
            );
        }
        // Credits stay pinned at the bottom (they never scroll with the
        // description).
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
        // Reset the scroll when the shown video changes. While a video
        // plays the key is the Jellyfin item id / mpv title; a YouTube
        // stream playing as audio keys by the queue song instead.
        let key = ctx
            .mpv
            .item_id
            .clone()
            .or_else(|| {
                (!crate::core::mpv::mpv_is_ui_source(ctx)).then(|| {
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

        // Hover: the link under the pointer lightens (the standard hover
        // effect), taking over from the terminal's URL underline.
        let hovered_link = ctx.mouse_pos().and_then(|pos| {
            if !list_area.contains(pos) {
                return None;
            }
            let idx = self.info_state.offset() + usize::from(pos.y - list_area.y);
            let x = pos.x.saturating_sub(list_area.x);
            (idx < body.len() && body[idx].link_ranges.iter().any(|(a, b)| *a <= x && x < *b))
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
        StatefulWidget::render(list, list_area, frame.buffer_mut(), &mut self.info_state);

        if scrollbar_area.width > 0
            && let Some(scrollbar) = ctx.config.as_styled_scrollbar()
        {
            let max_offset = self.info_items_len.saturating_sub(list_area.height as usize);
            let position = self.info_state.offset().min(max_offset);
            // content_length = max_offset + 1 so the bottom position is
            // reachable (ratatui clamps positions to content_length - 1);
            // the viewport length keeps the thumb proportional to the
            // visible rows.
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
        Line::from(vec![
            Span::styled(format!("{label}: "), key_style),
            Span::styled(value.to_owned(), list_style),
        ])
    }
}

impl Pane for LyricsPane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // The box needs at least 4 rows (title + context row + description
        // header + one content row). A smaller area renders nothing: the
        // squeezed layout would otherwise bleed the mpv/YouTube title onto
        // the box borders when the top split collapses at short window
        // heights (the info box "hidden" state). Responsive layouts
        // collapse the whole box (borders included) below this minimum —
        // see `panes::content_min_height` — this gate is the pane-level
        // defense (e.g. direct renders / tests).
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
        // A video that is the UI source: the box shows the video's
        // details (no lyrics, no MPD song).
        if crate::core::mpv::mpv_is_ui_source(ctx) {
            self.wrong_btn_area = Rect::default();
            self.fetch_btn_area = Rect::default();
            self.edit_btn_area = Rect::default();
            self.word_areas.clear();
            self.pressed_btn = None;
            self.render_mpv_info(frame, area, ctx);
            return Ok(());
        }
        // Lyrics/info combo: while paused the box shows the currently
        // playing song's details; while stopped (nothing playing) it shows
        // the highlighted item's details from the visible list; while
        // playing it shows the current song's details whenever no lyrics
        // are available, and the lyrics themselves when they are.
        if self.showing_info(ctx) {
            // Info mode has no lyrics buttons (nothing to mark wrong or
            // fetch): drop any stale click zones from a previous lyrics
            // render.
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
                // Nothing playing: the highlighted item from the list.
                selected()
            } else {
                // Playing or paused: the currently playing item (falling
                // back to the highlighted row when there is no current
                // song, e.g. paused with an empty status).
                ctx.find_current_song_in_queue()
                    .map(|(_, song)| song)
                    .or_else(selected)
            };
            // A resolved YouTube-style stream playing as audio through MPD
            // gets the same video-style details as the mpv box (title +
            // channel/subs + wrapped description), not the generic song
            // preview.
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

        // The lyrics view is framed: a blank top row, the body — the
        // lyrics, or the paused-style info panel while the current song's
        // lyrics are marked wrong — a margin line, then the `hide lyrics` /
        // `fetch lyrics` buttons on the bottom row (the label switches to
        // `show lyrics` while wrong-marked, so the buttons stay reachable
        // to bring the lyrics back).
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
        // Round 34: edit mode + paused renders the lyrics with every
        // word's timing next to it (raw file times), the selected word
        // highlighted, all visible lines editable. Resume keeps the normal
        // karaoke highlighting — edit mode only changes the paused view.
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
            // Skip blank timed lines (paragraph separators): a blank line has
            // empty content and renders nothing, and when it shares a timestamp
            // with the first line of the next verse it would win the tie-break
            // below, skipping that line's highlight entirely.
            .filter(|(_, line)| !line.content.is_empty() && elapsed >= line.time(offset))
            .min_by(|a, b| {
                a.1.time(offset).abs_diff(elapsed).cmp(&b.1.time(offset).abs_diff(elapsed))
            })
            .map_or((0, false), |result| (result.0, true));

        let rows = area.height;
        let areas = Layout::vertical((0..rows).map(|_| Constraint::Length(1))).split(area);
        let middle_row = rows.saturating_sub(1) / 2;

        let default_style = Style::default().fg(ctx.config.theme.text_color.unwrap_or_default());

        let middle_style = if first_line_reached {
            ctx.config.theme.highlighted_item_style
        } else {
            default_style
        };

        let timestamp = ctx.config.theme.lyrics.timestamp;

        let Some(current_line) = lrc.lines.get(current_line_idx) else {
            return Ok(());
        };

        // Render the current line. Lines with inline <mm:ss.xx> word markers
        // light up word by word (karaoke style); all other lines are
        // highlighted as a whole, exactly like before.
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
                format!("[{}] {}", current_line.time(offset).to_string(), current_line.content)
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
        let active_lyric_start_row =
            (middle_row as usize).saturating_sub(wrapped_lines_length.saturating_sub(1));
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

        while !areas.is_empty()
            && after_lyrics_cursor < lrc.lines.len() - 1
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

        // Try to schedule the next line change to be displayed on time: the
        // current line's start when it has not been reached yet (song
        // intro), otherwise the line after it.
        let next_idx = if first_line_reached { current_line_idx + 1 } else { current_line_idx };
        if self.last_requested_line_idx != next_idx
            && let Some(line) = lrc.lines.get(next_idx)
        {
            self.last_requested_line_idx = next_idx;
            ctx.scheduler
                .schedule(line.time(offset).saturating_sub(ctx.status.elapsed), run_status_update);
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

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::SongChanged | UiEvent::Reconnected => {
                // A new song invalidates the edit session: save any
                // pending edits and leave edit mode first.
                if self.edit_mode {
                    self.set_edit_mode(ctx, false)?;
                }
                // A song change clears the in-flight state. The held-button
                // marker is left alone: its lifecycle belongs to the press
                // / `LeftRelease` / release-check fallback alone (round 12
                // follow-up).
                self.fetching = false;
                if let Err(err) = self.update_lyrics(ctx) {
                    status_error!("Failed to load lyrics file: '{err}'");
                }
                ctx.render()?;
                // Nothing scheduled yet: the next render arms the schedule
                // for the current position.
                self.last_requested_line_idx = usize::MAX;
            }
            UiEvent::LyricsIndexed => {
                // A finished fetch — or an in-edit exact-time write from
                // the popup — landed here. In edit mode the session is
                // rebuilt from the saved file (staying in edit mode);
                // otherwise the lyrics reload.
                self.fetching = false;
                if let Err(err) = self.update_lyrics(ctx) {
                    status_error!("Failed to load lyrics file: '{err}'");
                }
                if self.edit_mode
                    && let Err(err) = self.rebuild_edit_session(ctx)
                {
                    status_error!("Failed to reload the lyrics editor: '{err}'");
                }
                ctx.render()?;
                self.last_requested_line_idx = usize::MAX;
            }
            UiEvent::PlaybackStateChanged => {
                // Pause/resume (or stop) invalidates the pending next-line
                // schedule: while paused the pane shows track info and never
                // re-arms it, so the highlight could otherwise lag after a
                // resume. Resetting here makes the first lyrics render after
                // the resume schedule the next line from the fresh position.
                self.last_requested_line_idx = usize::MAX;
                // Round 35: with edit mode on, pausing re-anchors the
                // selection to the word being sung at the pause position
                // (the user asked that the selection always sit on the
                // current lyric while paused).
                if self.edit_mode
                    && matches!(ctx.status.state, State::Pause)
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
        // Round 34: edit mode owns the keyboard while paused — word
        // navigation (`←`/`→`, `w`/`s`), nudge (`+`/`-`, 10 ms), the
        // exact-time popup (Enter), save (`<C-s>`), exit (Esc, saving).
        // Outside edit mode the pane claims nothing.
        if !self.edit_mode || self.edit_session.is_none() {
            return Ok(());
        }
        // Round 38 regression: Esc while edit mode is ON and the song is
        // PLAYING opened Settings instead of discarding — the old
        // paused-only gate let the key fall through to the global
        // ShowSettings half. The exit/save keys (Esc = discard,
        // `<C-c>` = save+exit, `<C-s>` = save in place) now work in ANY
        // play state while the lyrics pane is focused (handle_action is
        // only called for the focused pane); the rest of the edit key
        // set stays paused-gated and falls through to the global
        // handlers unchanged while playing.
        let paused = matches!(ctx.status.state, State::Pause);
        let claimed = event.claim_common();
        if !paused
            && !matches!(
                claimed,
                Some(CommonAction::Close)
                    | Some(CommonAction::LyricsSaveAndExit)
                    | Some(CommonAction::LyricsSave)
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
                // Round 37: Esc leaves edit mode WITHOUT saving (discard);
                // it consumes the key so the settings panel bound to the
                // same key does not open on the same press. `<C-s>` saves
                // in place, `<C-c>` saves and exits.
                self.discard_edit_mode(ctx)?;
                event.consume();
            }
            Some(CommonAction::LyricsSaveAndExit) => {
                // `<C-c>`: save the pending edits and leave edit mode.
                self.set_edit_mode(ctx, false)?;
            }
            Some(CommonAction::LyricsNudgeUp) => self.nudge_selection(ctx, 10)?,
            Some(CommonAction::LyricsNudgeDown) => self.nudge_selection(ctx, -10)?,
            Some(CommonAction::LyricsSave) => self.save_edit(ctx)?,
            // Round 35: `e` edits the line's text, `t` sets its
            // timestamp; round 41: `d` deletes the selected WORD (the
            // line is removed only when it ends up empty); round 40:
            // `i`/`a` insert a new WORD into the current line
            // (before/after the selected word), `o`/`O` add a whole new
            // line after/before the current one.
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
        // The lyrics buttons: `hide lyrics` toggles the current
        // song's wrong-mark (hides the lyrics — the body becomes the
        // paused-style info panel), `fetch lyrics` forces a refetch (and
        // clears the mark). Only active in lyrics mode — the zones are
        // reset to zero on every other render path, so a stale area never
        // accepts a click.
        //
        // The `⭘` pressed marker is pressed-while-held only: the press
        // arms the marker (and a release-check fallback); `LeftRelease`
        // (or the fallback) clears it. One action per press — a held
        // button never repeats just because the UI re-renders.
        match event.kind {
            MouseEventKind::LeftClick | MouseEventKind::DoubleClick => {
                let pos: ratatui::layout::Position = event.into();
                if self.edit_btn_area.contains(pos) {
                    self.pressed_btn = Some(LyricsBtn::Edit);
                    self.schedule_release_check(ctx);
                    return self.set_edit_mode(ctx, !self.edit_mode);
                }
                if self.wrong_btn_area.contains(pos) {
                    // `hide lyrics` hides the body — a contradictory state
                    // while editing, so leaving edit mode (saving) first.
                    if self.edit_mode {
                        self.set_edit_mode(ctx, false)?;
                    }
                    self.pressed_btn = Some(LyricsBtn::Wrong);
                    self.schedule_release_check(ctx);
                    return self.toggle_wrong(ctx);
                }
                if self.fetch_btn_area.contains(pos) {
                    // Fetching replaces the lyrics: leave edit mode
                    // (saving) first so the edit session cannot race the
                    // refetch.
                    if self.edit_mode {
                        self.set_edit_mode(ctx, false)?;
                    }
                    self.pressed_btn = Some(LyricsBtn::Fetch);
                    self.schedule_release_check(ctx);
                    return self.fetch_lyrics(ctx);
                }
                // Round 34: a click on a word in edit mode selects it.
                if self.edit_mode
                    && matches!(ctx.status.state, State::Pause)
                    && !self.is_wrong(ctx)
                    && let Some(area) = self.word_areas.iter().find(|w| w.rect.contains(pos))
                {
                    self.edit_selection = Some((area.line, area.word));
                    ctx.render()?;
                    return Ok(());
                }
            }
            MouseEventKind::LeftRelease => {
                // The press ended: the marker reverts to `●`. (The release
                // position is irrelevant — the marker follows the *held*
                // button, and a press on the button then a release
                // elsewhere still ends it.)
                return self.release_btn(ctx);
            }
            _ => {}
        }
        // A click / drag on the info view's scrollbar scrolls it (the thumb
        // follows the pointer 1:1). Also active while the lyrics are hidden
        // (wrong-marked): the body is the info panel then.
        if (self.showing_info(ctx) || self.is_wrong(ctx))
            && self.info_scrollbar_area.height > 0
            && matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. })
            && self.info_scrollbar_area.contains(event.into())
        {
            let max = self.info_items_len.saturating_sub(self.info_area.height as usize);
            if max > 0 {
                let viewport_len = self.info_area.height as usize;
                let position = self.info_state.offset();
                let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
                if let Some(perc) = self.info_scrollbar_drag.handle(
                    event,
                    self.info_scrollbar_area,
                    max + 1,
                    viewport_len,
                    position,
                    begin_len,
                    end_len,
                ) {
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
        // Scroll the track-info view (the lyrics view auto-follows the
        // current line and does not need scrolling). Also active while the
        // lyrics are hidden (wrong-marked): the body is the info panel then.
        if (self.showing_info(ctx) || self.is_wrong(ctx)) && self.info_area.contains(event.into()) {
            let dir = match event.kind {
                MouseEventKind::ScrollUp => -1,
                MouseEventKind::ScrollDown => 1,
                _ => return Ok(()),
            };
            let max =
                self.info_items_len.saturating_sub(self.info_area.height as usize) as i64;
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
            !(0x1F000..=0x1FAFF).contains(&cp) // emoji / pictographs
                && !(0x2600..=0x27BF).contains(&cp) // misc symbols + dingbats (✔ ★ …)
                && !(0xFE00..=0xFE0F).contains(&cp) // variation selectors
                && !(0x200D..=0x200D).contains(&cp) // zero-width joiner
                && !(0x20E3..=0x20E3).contains(&cp) // keycap
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
        Self { line: Line::from(spans), link_ranges }
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
                let is_link = self.link_ranges.iter().any(|(a, b)| *a == x && *b == x + w);
                x += w;
                if is_link {
                    Span::styled(span.content.clone(), crate::config::hover_style(span.style))
                } else {
                    span.clone()
                }
            })
            .collect();
        let mut line = Line::from(spans);
        line.alignment = self.line.alignment;
        line.style = self.line.style;
        Self { line, link_ranges: self.link_ranges.clone() }
    }
}

/// Split a line into spans, drawing `http(s)://` and `www.` URLs in the
/// link blue. Returns the spans and each link span's cell x-range within
/// the line (the pointer hit area for the hover lightening).
pub(crate) fn linkify_line(line: &str, base_style: Style) -> (Vec<Span<'static>>, Vec<(u16, u16)>) {
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
                // The body runs to the next whitespace; trailing punctuation
                // (commas, closing brackets, quotes — the way links appear
                // inside prose) is trimmed.
                let body = text[scheme_end..].split_whitespace().next().unwrap_or("");
                let trimmed = body.trim_end_matches(|c: char| {
                    matches!(
                        c,
                        '.' | ',' | ';' | ':' | '!' | '?' | ')' | ']' | '}' | '"' | '\'' | '»'
                    )
                });
                // A bare `www.` needs a real domain after it (at least one
                // dot), so prose like "see www.version 2.0" stays plain.
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
) -> (
    Vec<Span<'static>>,
    Vec<Span<'static>>,
    Vec<InfoBodyLine>,
) {
    let mut context_left: Vec<Span<'static>> = Vec::new();
    let mut context_right: Vec<Span<'static>> = Vec::new();
    let mut body: Vec<InfoBodyLine> = Vec::new();
    if let Some(channel) = yt.channel.as_deref().filter(|c| !c.is_empty()) {
        // The whole context line follows the theme color (the accent),
        // unlike the yellow group labels.
        context_left.push(Span::styled("Channel: ", base));
        context_left.push(Span::styled(channel.to_owned(), base));
    }
    if let Some(subs) = yt.subscribers.filter(|s| *s > 0) {
        context_right.push(Span::styled("Subs: ", base));
        context_right.push(Span::styled(compact_count(subs), base));
    }
    if let Some(description) = yt.description.as_deref().filter(|d| !d.trim().is_empty()) {
        // Emojis are scrubbed (they render as wide glyphs / tofu in the
        // box) and the text wraps to the box width; the body uses the
        // static list text color.
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


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::time::Duration;

    use ratatui::prelude::Rect;
    use ratatui::style::{Color, Modifier, Style};

    use super::{LINK_BLUE, LyricsBtn, LyricsPane, Pane, format_clock};
    use crate::{
        config::keys::CommonAction,
        mpd::commands::{Song, State},
        shared::{
            keys::{ActionEvent, Actions},
            mouse_event::{MouseEvent, MouseEventKind},
        },
        tests::fixtures::ctx,
    };

    fn song(id: u32, title: &str) -> Song {
        let mut song = Song {
            id,
            file: format!("/mnt/music/{title}.flac"),
            duration: Some(Duration::from_secs(10)),
            ..Default::default()
        };
        song.metadata.insert("title".to_string(), title.into());
        song.metadata.insert("artist".to_string(), "Test Artist".into());
        song
    }

    fn rendered(pane: &mut LyricsPane, ctx: &mut crate::ctx::Ctx, width: u16, height: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, width, height), ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn fixture() -> (crate::ctx::Ctx, LyricsPane) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = vec![song(1, "First"), song(2, "Second"), song(3, "Third")];
        let pane = LyricsPane::new(&ctx);
        (ctx, pane)
    }

    /// A playing fixture with lyrics loaded for the current song (id 1):
    /// the pane is in lyrics mode with the header row.
    fn lyrics_fixture() -> (crate::ctx::Ctx, LyricsPane) {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        ctx.status.elapsed = Duration::from_millis(1500);
        pane.current_lyrics = Some(
            "[ti:First]\n[ar:Test Artist]\n[00:01.00]line one\n[00:02.00]line two\n"
                .parse()
                .unwrap(),
        );
        (ctx, pane)
    }

    fn click(pane: &mut LyricsPane, ctx: &crate::ctx::Ctx, area: ratatui::layout::Rect) {
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x,
                y: area.y,
                kind: MouseEventKind::LeftClick,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            ctx,
        )
        .unwrap();
    }

    /// A full press+release cycle on the button (a real click).
    fn click_and_release(
        pane: &mut LyricsPane,
        ctx: &crate::ctx::Ctx,
        area: ratatui::layout::Rect,
    ) {
        click(pane, ctx, area);
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x,
                y: area.y,
                kind: MouseEventKind::LeftRelease,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            ctx,
        )
        .unwrap();
    }

    /// The style of one cell of the last render.
    fn cell_style(
        pane: &mut LyricsPane,
        ctx: &mut crate::ctx::Ctx,
        width: u16,
        height: u16,
        x: u16,
        y: u16,
    ) -> ratatui::style::Style {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, width, height), ctx).unwrap())
            .unwrap();
        terminal.backend().buffer()[(x, y)].style()
    }


    #[test]
    fn lyrics_mode_shows_the_button_cluster_unpressed() {
        let (mut ctx, mut pane) = lyrics_fixture();
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        // No `Artist - Title` header (round 11): the top row carries only
        // the two-button cluster with the `|` separator.
        assert!(!text.contains("Test Artist - First"), "no title header: {text}");
        assert!(
            text.contains("● hide lyrics | ● fetch lyrics"),
            "cluster with separator: {text}"
        );
        assert!(!text.contains("⭘"), "no pressed marker without interaction: {text}");
        assert!(text.contains("line one"), "the lyrics body is shown: {text}");
        assert!(
            pane.wrong_btn_area.width > 0 && pane.fetch_btn_area.width > 0,
            "click zones recorded"
        );
        // One-cell right margin (round 12): the cluster's last cell is one
        // in from the right border.
        assert_eq!(
            pane.fetch_btn_area.right() + 1,
            60,
            "cluster sits one cell in from the right border"
        );
        // Round 13 layout: a blank top row, the body, then the bottom
        // margin line and the buttons on the last row. The top row carries
        // no characters — the lyrics start one row below the pane's top
        // border.
        assert_eq!(pane.wrong_btn_area.y, 11, "buttons sit on the bottom row");
        let lines: Vec<&str> = text.lines().collect();
        assert!(
            lines[0].chars().all(char::is_whitespace),
            "top row is blank: {:?}",
            lines[0]
        );
        assert!(
            lines[10].chars().all(|c| c == '─'),
            "bottom margin line spans the width: {:?}",
            lines[10]
        );
        assert!(
            lines[11].contains("● hide lyrics | ● fetch lyrics"),
            "cluster on the bottom row: {:?}",
            lines[11]
        );
        assert_eq!(
            lines[11].chars().nth(58),
            Some('s'),
            "cluster one cell in from the right border: {:?}",
            lines[11]
        );
        assert!(
            lines[1..10].iter().any(|l| l.contains("line one")),
            "the lyrics body sits between the margin lines: {text}"
        );
    }

    #[test]
    fn header_buttons_hidden_in_info_mode() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(!text.contains("hide lyrics"), "no buttons in info mode: {text}");
        assert!(!text.contains("show lyrics"), "no show button in info mode: {text}");
        assert!(!text.contains("fetch lyrics"), "no fetch button in info mode: {text}");
        assert_eq!(pane.wrong_btn_area.width, 0, "stale click zone cleared");
    }

    #[test]
    fn wrong_lyrics_button_marks_and_hides_the_lyrics() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;
        click_and_release(&mut pane, &ctx, area);
        assert!(
            pane.wrong_song_file.as_deref() == Some("/mnt/music/First.flac"),
            "the current song is marked wrong"
        );
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        // The `⭘` marker is pressed-while-held only: after the release it
        // reverts to `●`; the button label switches to `show lyrics` (the
        // hidden lyrics are the wrong-mark indicator).
        assert!(!text.contains("⭘"), "marker is not persistent: {text}");
        assert!(text.contains("● show lyrics"), "show button unpressed: {text}");
        assert!(text.contains("● fetch lyrics"), "fetch stays unpressed: {text}");
        assert!(
            !text.contains("line one") && !text.contains("line two"),
            "the lyrics body is hidden: {text}"
        );
        assert!(
            text.contains("fetch lyrics"),
            "the buttons stay so fetch remains reachable: {text}"
        );
        // Round 12: the hidden lyrics show the paused-style info panel
        // (the current song's metadata) in the body area.
        assert!(text.contains("Title"), "the info panel shows metadata labels: {text}");
        assert!(text.contains("First"), "the info panel shows the current song: {text}");
    }

    #[test]
    fn wrong_lyrics_button_toggles_back() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;
        click_and_release(&mut pane, &ctx, area);
        click_and_release(&mut pane, &ctx, area);
        assert!(pane.wrong_song_file.is_none(), "second click clears the mark");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("● hide lyrics"), "back to hide: {text}");
        assert!(text.contains("line one"), "the lyrics body is back: {text}");
    }

    #[test]
    fn fetch_lyrics_clears_the_wrong_mark_and_shows_lyrics_again() {
        let (mut ctx, mut pane) = lyrics_fixture();
        // A harmless external command (true) so the fetch thread exits
        // cleanly; the fetched result is irrelevant to this assertion.
        ctx.config =
            std::sync::Arc::new(crate::config::Config {
                on_song_change: Some(std::sync::Arc::new(vec!["true".to_owned()])),
                ..crate::config::Config::default()
            });
        rendered(&mut pane, &mut ctx, 60, 12);
        let wrong_area = pane.wrong_btn_area;
        click(&mut pane, &ctx, wrong_area);
        assert!(
            pane.wrong_song_file.is_some(),
            "precondition: marked wrong"
        );
        rendered(&mut pane, &mut ctx, 60, 12);
        let fetch_area = pane.fetch_btn_area;
        click_and_release(&mut pane, &ctx, fetch_area);
        assert!(
            pane.wrong_song_file.is_none(),
            "fetch clears the wrong-mark immediately"
        );
        assert!(pane.fetching, "fetch is in flight");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        // The fetch in-flight state has no persistent marker either: only a
        // held press shows `⭘`.
        assert!(!text.contains("⭘"), "no persistent in-flight marker: {text}");
        assert!(
            text.contains("● fetch lyrics"),
            "fetch button shows unpressed after release: {text}"
        );
        assert!(
            text.contains("line one"),
            "clearing the mark shows the (current) lyrics again: {text}"
        );
        // The index reload (LyricsIndexed) ends the in-flight state.
        pane.on_event(&mut crate::ui::UiEvent::LyricsIndexed, true, &ctx).unwrap();
        assert!(!pane.fetching, "LyricsIndexed clears the in-flight marker");
    }

    #[test]
    fn narrow_panes_collapse_the_buttons_then_hide_them() {
        let (mut ctx, mut pane) = lyrics_fixture();
        // Wide: the full cluster fits.
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            text.contains("● hide lyrics | ● fetch lyrics"),
            "full labels at wide width: {text}"
        );
        assert!(pane.wrong_btn_area.width > 0, "click zones recorded");

        // Narrow: the full cluster no longer fits, the short labels do.
        let text = rendered(&mut pane, &mut ctx, 28, 12);
        assert!(
            text.contains("● hide | ● fetch"),
            "collapsed labels at narrow width: {text}"
        );
        assert!(!text.contains("hide lyrics"), "full label hidden when collapsed: {text}");
        assert!(pane.wrong_btn_area.width > 0, "collapsed click zones recorded");

        // Very narrow: even the collapsed cluster does not fit — hidden.
        let text = rendered(&mut pane, &mut ctx, 12, 12);
        assert!(!text.contains("hide"), "buttons hidden at very narrow width: {text}");
        assert!(!text.contains("fetch"), "buttons hidden at very narrow width: {text}");
        assert_eq!(pane.wrong_btn_area.width, 0, "no click zone when hidden");
        assert_eq!(pane.fetch_btn_area.width, 0, "no fetch click zone when hidden");
    }

    #[test]
    fn pressed_marker_shows_only_while_the_button_is_held() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;

        // Press: the marker shows while held (and the action fired once).
        // The press also wrong-marks the song, so the label is already
        // `show lyrics`.
        click(&mut pane, &ctx, area);
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            text.contains("⭘ show lyrics"),
            "⭘ while held: {text}"
        );
        assert!(text.contains("● fetch lyrics"), "fetch stays unpressed: {text}");

        // Release: the marker reverts to ● (the wrong-mark stays hidden).
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x,
                y: area.y,
                kind: MouseEventKind::LeftRelease,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("● show lyrics"), "back to ● after release: {text}");
        assert!(!text.contains("⭘"), "no ⭘ after release: {text}");
        assert!(pane.pressed_btn.is_none(), "pressed state cleared");
    }

    #[test]
    fn release_check_fallback_reverts_the_pressed_marker() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;
        click(&mut pane, &ctx, area);
        assert_eq!(pane.pressed_btn, Some(LyricsBtn::Wrong), "pressed while held");
        // Terminals without release events never send `LeftRelease`: the
        // scheduled fallback fires instead and reverts the marker.
        pane.release_btn(&ctx).unwrap();
        assert!(pane.pressed_btn.is_none(), "fallback cleared the marker");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("● show lyrics"), "back to ●: {text}");
    }

    #[test]
    fn held_press_keeps_the_marker_across_renders() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;
        // Round 12 (the kitty bug): on a release-reporting terminal the
        // release-check one-shot never fires mid-hold, so the `⭘` marker
        // persists for the whole hold. The unit-test environment's
        // emulator is `Unknown`, so the fallback is still scheduled but
        // dormant (the scheduler never advances here) — the press alone
        // keeps the marker across renders.
        click(&mut pane, &ctx, area);
        for _ in 0..3 {
            let text = rendered(&mut pane, &mut ctx, 60, 12);
            assert_eq!(pane.pressed_btn, Some(LyricsBtn::Wrong), "pressed while held");
            assert!(text.contains("⭘ show lyrics"), "⭘ persists across renders: {text}");
        }
        // The release (or the fallback) still ends the marker.
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x,
                y: area.y,
                kind: MouseEventKind::LeftRelease,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert!(pane.pressed_btn.is_none(), "release ends the hold");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("● show lyrics"), "back to ● after the hold: {text}");
    }

    #[test]
    fn fetch_press_keeps_the_marker_across_the_fetch_completion() {
        // Round 12 follow-up: the fetch button lost its `⭘` mid-hold —
        // the fetch-completion event (LyricsIndexed) cleared the
        // held-button marker while the mouse was still down. The marker's
        // lifecycle belongs to the press/release/fallback alone.
        let (mut ctx, mut pane) = lyrics_fixture();
        // A harmless external command (true) so the fetch thread exits
        // cleanly; the fetched result is irrelevant to these assertions.
        ctx.config =
            std::sync::Arc::new(crate::config::Config {
                on_song_change: Some(std::sync::Arc::new(vec!["true".to_owned()])),
                ..crate::config::Config::default()
            });
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.fetch_btn_area;
        click(&mut pane, &ctx, area);
        assert_eq!(pane.pressed_btn, Some(LyricsBtn::Fetch), "pressed while held");
        assert!(pane.fetching, "fetch is in flight");
        // The fetch completes mid-hold (index reload): the marker must
        // survive and persist across renders.
        pane.on_event(&mut crate::ui::UiEvent::LyricsIndexed, true, &ctx).unwrap();
        assert!(!pane.fetching, "LyricsIndexed ends the in-flight state");
        assert_eq!(
            pane.pressed_btn,
            Some(LyricsBtn::Fetch),
            "held marker survives the fetch completion"
        );
        // The index reload in the test env finds no .lrc file and drops
        // the lyrics (update_lyrics) — restore them (in production the
        // completed fetch created the file) so the pane renders the
        // button row again.
        pane.current_lyrics = Some(
            "[ti:First]\n[ar:Test Artist]\n[00:01.00]line one\n[00:02.00]line two\n"
                .parse()
                .unwrap(),
        );
        for _ in 0..3 {
            let text = rendered(&mut pane, &mut ctx, 60, 12);
            assert!(text.contains("⭘ fetch lyrics"), "⭘ persists across renders: {text}");
        }
        // The release still ends the marker.
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x,
                y: area.y,
                kind: MouseEventKind::LeftRelease,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert!(pane.pressed_btn.is_none(), "release ends the hold");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("● fetch lyrics"), "back to ● after the hold: {text}");
    }

    #[test]
    fn release_reporting_terminals_skip_the_fallback() {
        use crate::shared::terminal::Emulator;
        // The kitty-family emulators report mouse release events: they must
        // rely on `LeftRelease` alone (no 300 ms one-shot that could fire
        // mid-hold and flash the marker back to `●`).
        for emul in [
            Emulator::Kitty,
            Emulator::Ghostty,
            Emulator::WezTerm,
            Emulator::Konsole,
            Emulator::Foot,
            Emulator::Iterm2,
        ] {
            assert!(LyricsPane::reports_mouse_release(emul), "{emul} reports releases");
        }
        // Unknown / VSCode / Tabby keep the fallback so the marker never
        // sticks when releases are not reported.
        for emul in [Emulator::Unknown, Emulator::VSCode, Emulator::Tabby] {
            assert!(!LyricsPane::reports_mouse_release(emul), "{emul} keeps the fallback");
        }
    }

    #[test]
    fn hover_highlights_only_the_label_text() {
        let (mut ctx, mut pane) = lyrics_fixture();
        rendered(&mut pane, &mut ctx, 60, 12);
        let area = pane.wrong_btn_area;
        let base = ctx.config.as_text_style();
        let highlight = ctx.config.theme.hovered_item_style;

        // Point the mouse at the hide button: only the label text gets the
        // queue-list row highlight (bg + bold) and the brightening — the
        // `●`/`⭘` glyph keeps its completely normal style (round 12).
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: area.x, y: area.y }));
        let glyph_style = cell_style(&mut pane, &mut ctx, 60, 12, area.x, area.y);
        assert_eq!(glyph_style.fg, base.fg, "glyph keeps its normal fg");
        assert_eq!(glyph_style.bg, Some(Color::Reset), "glyph has no hover background");
        assert!(
            !glyph_style.add_modifier.contains(Modifier::BOLD),
            "glyph is not bolded by the hover"
        );
        // The separator space between the glyph and the text keeps the
        // normal style too: the highlight covers `hide lyrics` only, not
        // ` hide lyrics` (round 12 follow-up).
        let space_style = cell_style(&mut pane, &mut ctx, 60, 12, area.x + 1, area.y);
        assert_eq!(space_style.fg, base.fg, "separator space keeps the normal fg");
        assert_eq!(
            space_style.bg,
            Some(Color::Reset),
            "separator space has no hover background"
        );
        assert!(
            !space_style.add_modifier.contains(Modifier::BOLD),
            "separator space is not bolded by the hover"
        );

        let label_style = cell_style(&mut pane, &mut ctx, 60, 12, area.x + 2, area.y);
        assert_eq!(label_style.bg, highlight.bg, "label gets the hover bg");
        assert_eq!(
            label_style.fg,
            crate::config::hover_style(base).fg,
            "label text is brightened"
        );
        assert!(
            label_style.add_modifier.contains(Modifier::BOLD),
            "label gets bold from the hover"
        );

        // The separator keeps the plain base style (one cell past the
        // wrong button: `start + wrong_w`). The buffer cells carry the
        // default bg/underline (Reset), so compare the fields the app
        // actually sets.
        let sep_style = cell_style(&mut pane, &mut ctx, 60, 12, area.right(), area.y);
        assert_eq!(sep_style.fg, base.fg, "separator keeps the base foreground");
        assert_eq!(sep_style.bg, Some(Color::Reset), "separator has no hover background");

        ctx.set_mouse_pos(None);
        let plain = cell_style(&mut pane, &mut ctx, 60, 12, area.x, area.y);
        assert_eq!(plain.bg, Some(Color::Reset), "no hover highlight without the mouse");
    }

    #[test]
    fn paused_shows_the_current_song_info() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(3);
        // The queue selection is on "Second", but paused shows the
        // currently playing item.
        ctx.queue_selected_id.set(Some(2));
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("Third"), "paused: the current song's info is shown");
        assert!(!text.contains("Second"));
        assert!(text.contains("Title"), "the info includes the metadata labels");
    }

    #[test]
    fn stopped_shows_the_highlighted_song_info() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Stop;
        ctx.queue_selected_id.set(Some(2));
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("Second"), "stopped: the highlighted song's info is shown");
        assert!(!text.contains("First"));
    }

    /// A resolved YouTube-style stream playing as **audio through MPD**
    /// gets the same video-style details as the mpv box (title, channel,
    /// "Description ↴" + wrapped body) instead of the generic song
    /// preview groups.
    #[test]
    fn youtube_stream_audio_uses_the_video_info_layout() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Play;
        let stream_url = "https://rr4.example/audio.m4a".to_owned();
        ctx.queue = vec![Song {
            id: 9,
            file: stream_url.clone(),
            duration: None,
            ..Default::default()
        }];
        ctx.status.songid = Some(9);
        ctx.status.duration = std::time::Duration::from_secs(710);
        ctx.yt_info.borrow_mut().insert(
            stream_url.clone(),
            crate::shared::ytdlp::YtStreamInfo {
                url: stream_url,
                original_url: "https://youtu.be/abc".to_owned(),
                title: "Some Mix".to_owned(),
                channel: Some("Some Channel".to_owned()),
                description: Some(
                    "A long description that wraps around inside the box instead of one long line."
                        .to_owned(),
                ),
                ..Default::default()
            },
        );
        let text = rendered(&mut pane, &mut ctx, 46, 14);
        assert!(text.contains("Some Mix"), "cached title shown: {text}");
        assert!(text.contains("Some Channel"), "channel row shown: {text}");
        assert!(text.contains("Description"), "video-style description label: {text}");
        assert!(
            text.contains("wraps around"),
            "description body shown: {text}"
        );
        assert!(
            text.lines().all(|row| row.chars().count() <= 46),
            "description wraps, no row overflows the box"
        );
        // Not the generic song-preview layout.
        assert!(!text.contains("--- [YouTube]"), "no preview group header: {text}");
        assert!(!text.contains("Last Modified"), "no preview metadata rows: {text}");
        assert!(!text.contains("Filename"), "no preview filename row: {text}");
    }

    #[test]
    fn paused_without_a_current_song_falls_back_to_the_selection() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.status.songid = None;
        ctx.queue_selected_id.set(Some(2));
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("Second"), "paused without a current song: the selection's info");
    }

    #[test]
    fn playing_without_lyrics_shows_the_current_song_info() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("First"), "playing without lyrics: the current song's info");
    }

    #[test]
    /// The video info box's scrolling title (a YouTube video via mpv)
    /// renders in explicit ANSI white, never the auto/blur accent.
    #[test]
    fn video_info_scrolling_title_is_white() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.duration = 710.0;
        ctx.mpv.title = "Maglev Keyboard Test".to_owned();
        ctx.yt_info.borrow_mut().insert(
            "https://youtu.be/abc".to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Maglev Keyboard Test".to_owned(),
                ..Default::default()
            },
        );
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "Maglev Keyboard Test",
            "https://youtu.be/abc",
            Some(710.0),
        ));
        ctx.mpv.playlist_pos.set(Some(0));

        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 60, 12), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut title_cell_white = false;
        for y in 0..2u16 {
            let row: String = (0..60u16)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if let Some(x0) = row.find("Maglev Keyboard Test") {
                title_cell_white = buf[(x0 as u16, y)].style().fg
                    == Some(ratatui::style::Color::White);
                break;
            }
        }
        assert!(title_cell_white, "the scrolling video title renders white");
    }

    #[test]
    fn local_file_title_keeps_the_list_color_in_the_info_box() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        ctx.queue = vec![song(1, "Local Track")];

        let backend = ratatui::backend::TestBackend::new(60, 12);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 60, 12), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let mut title_cell_white = false;
        for y in 0..12u16 {
            let row: String = (0..60u16)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            if let Some(x0) = row.find("Title: Local Track") {
                title_cell_white = buf[((x0 + "Title: ".len()) as u16, y)].style().fg
                    == Some(ratatui::style::Color::White);
                break;
            }
        }
        assert!(!title_cell_white, "a local file's title keeps the list color");
    }

    #[test]
    fn video_playing_shows_the_video_details() {
        use crate::core::mpv::{MpvPlaylistEntry, MpvSession};
        let (mut ctx, mut pane) = fixture();
        // A Jellyfin episode playing in mpv (MPD is paused meanwhile).
        ctx.status.state = State::Pause;
        ctx.mpv = MpvSession {
            active: true,
            item_id: Some("abcdef0123456789abcdef0123456789".to_owned()),
            title: "The Pilot".to_owned(),
            artist: "The Show".to_owned(),
            duration: 2700.0,
            item: Some(crate::jellyfin::JfItem {
                id: "abcdef0123456789abcdef0123456789".to_owned(),
                name: "The Pilot".to_owned(),
                kind: "Episode".to_owned(),
                series_name: Some("The Show".to_owned()),
                season_number: Some(3),
                index_number: Some(3),
                year: Some(2024),
                runtime_secs: Some(2700),
                ..Default::default()
            }),
            playlist: std::cell::RefCell::new(vec![MpvPlaylistEntry::new(
                "The Pilot",
                "http://jf/Videos/abcdef0123456789abcdef0123456789/stream",
                Some(2700.0),
            )]),
            playlist_pos: std::cell::Cell::new(Some(0)),
            ..Default::default()
        };
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("2024 -- The Show"), "year + series header: {text}");
        assert!(text.contains("The Pilot"), "the episode name is shown: {text}");
        assert!(text.contains("S03E03"), "the season/episode tag is shown: {text}");
        assert!(text.contains("45:00"), "the runtime is shown: {text}");
        assert!(text.contains("Time"), "the header shows the bold time: {text}");
        // Not the queue's songs.
        assert!(!text.contains("First"));
    }

    #[test]
    fn video_info_scrubs_emoji_and_shows_subscriber_count() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.duration = 710.0;
        ctx.yt_info.borrow_mut().insert(
            "https://youtu.be/abc".to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Maglev Keyboard Test".to_owned(),
                channel: Some("Hipyo Tech".to_owned()),
                subscribers: Some(1_710_000),
                description: Some("Check it out! \u{2714} Buy keycaps here \u{2714}".to_owned()),
                ..Default::default()
            },
        );
        // The session plays the stream (mpv_yt_info looks the playlist
        // entry's URL up).
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "Maglev Keyboard Test",
            "https://youtu.be/abc",
            Some(710.0),
        ));
        ctx.mpv.playlist_pos.set(Some(0));

        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("Maglev Keyboard Test"), "title: {text}");
        assert!(text.contains("Channel: Hipyo Tech"), "channel row: {text}");
        assert!(text.contains("Subs: 1.71M"), "subscriber count: {text}");
        assert!(text.contains("Description"), "description label: {text}");
        assert!(text.contains("Buy keycaps here"), "description body: {text}");
        assert!(!text.contains("\u{2714}"), "emoji must be scrubbed: {text}");
    }

    /// The info box needs 4 rows (title + context row + description header
    /// + one content row): below that nothing is rendered — the squeezed
    /// layout would otherwise bleed the mpv/YouTube title onto the box
    /// borders when the queue tab's top split collapses at short window
    /// heights.
    #[test]
    fn mpv_info_box_hidden_below_four_rows() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.duration = 710.0;
        ctx.yt_info.borrow_mut().insert(
            "https://youtu.be/abc".to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "A Long Scrolling YouTube Title".to_owned(),
                channel: Some("Some Channel".to_owned()),
                ..Default::default()
            },
        );
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "A Long Scrolling YouTube Title",
            "https://youtu.be/abc",
            Some(710.0),
        ));
        ctx.mpv.playlist_pos.set(Some(0));

        for height in [0u16, 1, 2, 3] {
            let text = rendered(&mut pane, &mut ctx, 60, height);
            let rendered_chars = text.chars().filter(|c| *c != ' ' && *c != '\n').count();
            assert_eq!(rendered_chars, 0, "nothing renders at {height} rows: {text:?}");
        }
        // At 4 rows the box comes back.
        let text = rendered(&mut pane, &mut ctx, 60, 4);
        assert!(text.contains("A Long Scrolling YouTube Title"), "title at 4 rows: {text}");
    }

    /// The same 4-row minimum applies to the song-info / lyrics content.
    #[test]
    fn song_info_box_hidden_below_four_rows() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(2);
        let text = rendered(&mut pane, &mut ctx, 60, 3);
        let rendered_chars = text.chars().filter(|c| *c != ' ' && *c != '\n').count();
        assert_eq!(rendered_chars, 0, "nothing renders at 3 rows: {text:?}");
        let text = rendered(&mut pane, &mut ctx, 60, 4);
        assert!(text.contains("Second"), "song info at 4 rows: {text}");
    }

    #[test]
    fn title_marquee_holds_static_then_scrolls() {
        use std::time::{Duration, Instant};
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.duration = 710.0;
        // A title too long to fit the box.
        let long =
            "The Maglev Keyboard And It Was INSANE \u{2014} a very long title that overflows";
        ctx.mpv.title = long.to_owned();
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            long,
            "https://youtu.be/abc",
            Some(710.0),
        ));
        ctx.mpv.playlist_pos.set(Some(0));

        // Freshly shown: the 2s static pause shows the title start.
        let static_text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            static_text.starts_with("The Maglev Keyboard"),
            "static pause shows the title start: {static_text}"
        );

        // After the pause (wall clock) the title scrolls.
        pane.info_video_shown_at = Some(Instant::now() - Duration::from_millis(3000));
        let scrolled = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            !scrolled.starts_with("The Maglev"),
            "must not show the title start while scrolling: {scrolled}"
        );
        assert_ne!(
            scrolled.lines().next(),
            static_text.lines().next(),
            "the title row shifted after the pause"
        );

        // A title that fits the area never marquee-scrolls, even long after
        // the pause.
        ctx.mpv.title = "Short Title".to_owned();
        pane.info_video_shown_at = Some(Instant::now() - Duration::from_secs(60));
        let fitting = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            fitting.starts_with("Short Title"),
            "fitting titles stay static: {fitting}"
        );
    }

    #[test]
    fn format_clock_keeps_hours_for_long_durations() {
        assert_eq!(format_clock(5393), "1:29:53");
        assert_eq!(format_clock(710), "11:50");
        assert_eq!(format_clock(3600), "1:00:00");
        assert_eq!(format_clock(59), "0:59");
    }

    #[test]
    fn clicking_the_info_scrollbar_jumps_to_the_position() {
        use crossterm::event::KeyModifiers;
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv.active = true;
        ctx.mpv.duration = 710.0;
        // A long description overflows the scrollable body.
        let description = (0..40)
            .map(|i| format!("paragraph line {i} with some words to wrap"))
            .collect::<Vec<_>>()
            .join(" ");
        ctx.yt_info.borrow_mut().insert(
            "https://youtu.be/abc".to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "A Long Scrolling Title".to_owned(),
                description: Some(description),
                ..Default::default()
            },
        );
        ctx.mpv.playlist.borrow_mut().push(crate::core::mpv::MpvPlaylistEntry::new(
            "A Long Scrolling Title",
            "https://youtu.be/abc",
            Some(710.0),
        ));
        ctx.mpv.playlist_pos.set(Some(0));
        rendered(&mut pane, &mut ctx, 60, 12);
        assert!(
            pane.info_scrollbar_area.height > 0,
            "the description must overflow and show a scrollbar"
        );

        // A click at the bottom of the scrollbar jumps to the end: track
        // clicks put the thumb's top under the pointer, and the bottom row
        // of the bar is past the thumb's travel, so the offset reaches max.
        let sb = pane.info_scrollbar_area;
        let max = pane.info_items_len.saturating_sub(pane.info_area.height as usize);
        assert!(max > 0);
        pane.handle_mouse_event(
            MouseEvent {
                x: sb.x,
                y: sb.y + sb.height - 1,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.info_state.offset(), max, "click jumps the offset");
        assert!(pane.info_state.offset() > 0, "near the end of the list");
    }

    #[test]
    fn description_wraps_to_the_box_width() {
        let (mut ctx, mut pane) = fixture();
        ctx.status.state = State::Pause;
        ctx.mpv = crate::core::mpv::MpvSession {
            active: true,
            playlist: std::cell::RefCell::new(vec![crate::core::mpv::MpvPlaylistEntry::new(
                "A Video",
                "https://www.youtube.com/watch?v=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                None,
            )]),
            playlist_pos: std::cell::Cell::new(Some(0)),
            ..Default::default()
        };
        let long = "one two three four five six seven eight nine ten";
        ctx.yt_info.borrow_mut().insert(
            "https://www.youtube.com/watch?v=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
            crate::shared::ytdlp::YtStreamInfo {
                title: "A Video".to_owned(),
                original_url:
                    "https://www.youtube.com/watch?v=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned(),
                description: Some(long.to_owned()),
                ..Default::default()
            },
        );
        let text = rendered(&mut pane, &mut ctx, 30, 20);
        assert!(text.contains("Description"), "description label shown: {text}");
        // The description text wraps instead of overflowing: every row is at
        // most the box width.
        for row in text.lines() {
            assert!(row.chars().count() <= 30, "row overflows: {row:?}");
        }
        assert!(text.contains("one two three"), "words wrap across rows");
    }

    #[test]
    fn info_scrollbar_uses_the_theme_symbols() {
        let (mut ctx, mut pane) = fixture();
        // A distinct scrollbar style: if anything resets the symbols to
        // ratatui's defaults (▲/█/║/▼ — e.g. `Scrollbar::orientation()`),
        // the rendered column stops matching these.
        let theme: crate::config::theme::UiConfigFile = ron::from_str(
            r#"#![enable(implicit_some)]
#![enable(unwrap_variant_newtypes)]
(
                scrollbar: (
                    symbols: ["─", "●", "◤", "◢"],
                    track_style: (),
                    ends_style: (),
                    thumb_style: (),
                ),
            )"#,
        )
        .unwrap();
        let mut config = crate::config::Config::default();
        config.theme = theme.try_into().unwrap();
        ctx.config = std::sync::Arc::new(config);
        // Paused: the box shows the queue selection's info; many tags make
        // it overflow so the scrollbar renders.
        ctx.status.state = State::Pause;
        let mut song = crate::mpd::commands::Song {
            id: 99,
            file: "/mnt/music/a.flac".to_owned(),
            ..Default::default()
        };
        for i in 0..40 {
            song.metadata.insert(format!("tag{i}"), format!("value {i}").into());
        }
        ctx.queue.push(song);
        ctx.queue_selected_id.set(Some(99));

        let backend = ratatui::backend::TestBackend::new(60, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 60, 20), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();

        let col: Vec<String> =
            (0..20u16).map(|y| buf[(59, y)].symbol().to_string()).collect();
        // The scrollbar keeps the theme's begin/end/track symbols (the
        // ratatui defaults would be ▲/║/▼ — `║` proves the reset bug).
        assert_eq!(col[0], "◤", "begin symbol: {col:?}");
        assert_eq!(*col.last().unwrap(), "◢", "end symbol: {col:?}");
        assert!(
            col[1..col.len() - 1].iter().all(|c| c == "─" || c == "●"),
            "track/thumb symbols: {col:?}"
        );
        assert!(!col.iter().any(|c| c == "║"), "ratatui default track leaked: {col:?}");
    }

    #[test]
    fn find_url_finds_schemes_and_trims_trailing_punctuation() {
        use super::find_url;
        // Plain https/http schemes.
        assert_eq!(find_url("https://example.com"), Some((0, 19)));
        assert_eq!(find_url("http://example.com"), Some((0, 18)));
        // Case-insensitive scheme.
        assert_eq!(find_url("HTTPS://example.com"), Some((0, 19)));
        // Trailing punctuation belongs to the prose, not the URL.
        assert_eq!(find_url("Watch: https://youtu.be/abc123."), Some((7, 23)));
        assert_eq!(find_url("(https://youtu.be/abc123)"), Some((1, 23)));
        assert_eq!(find_url("see https://example.com/?a=1,"), Some((4, 24)));
        // A scheme with no body is not a link.
        assert_eq!(find_url("https://"), None);
        // No scheme: nothing.
        assert_eq!(find_url("example.com is a domain"), None);
        // The link after leading text.
        assert_eq!(find_url("text https://a.com rest"), Some((5, 13)));
        // Bare www. links (no scheme), with a real domain after them.
        assert_eq!(find_url("visit www.example.com today"), Some((6, 15)));
        assert_eq!(find_url("www.youtube.com/watch?v=abc"), Some((0, 27)));
        // A bare www. without a domain is prose, not a link.
        assert_eq!(find_url("see www.version 2.0 notes"), None);
        assert_eq!(find_url("www. "), None);
    }

    #[test]
    fn linkify_line_styles_urls_blue_and_reports_cell_ranges() {
        use super::linkify_line;
        let base = Style::default().fg(Color::Rgb(0xaa, 0xaa, 0xaa));
        let (spans, ranges) = linkify_line("a https://x.com b", base);
        assert_eq!(spans.len(), 3);
        assert_eq!(spans[1].content.as_ref(), "https://x.com");
        assert_eq!(spans[1].style.fg, Some(LINK_BLUE));
        // The link's cell range: after "a " (2 cells), 13 wide.
        assert_eq!(ranges, vec![(2, 15)]);
        // The surrounding text keeps the base style.
        assert_eq!(spans[0].content.as_ref(), "a ");
        assert_eq!(spans[0].style, base);
        assert_eq!(spans[2].content.as_ref(), " b");
    }

    #[test]
    fn linkify_line_offsets_wide_chars_before_the_link() {
        use super::linkify_line;
        let base = Style::default();
        // 絵 is 2 cells wide: the link starts at cell 3.
        let (spans, ranges) = linkify_line("絵 https://x.com", base);
        assert_eq!(ranges, vec![(3, 16)]);
        assert_eq!(spans[0].content.as_ref(), "絵 ");
        assert_eq!(spans[1].content.as_ref(), "https://x.com");
    }

    #[test]
    fn hovered_row_lightens_only_the_link_spans() {
        use super::InfoBodyLine;
        let base = Style::default().fg(Color::Rgb(0xaa, 0xaa, 0xaa));
        let row = InfoBodyLine::new("a https://x.com b", base);
        let hovered = row.hovered();
        let spans = &hovered.line.spans;
        assert_eq!(spans.len(), 3);
        // The link span lightens (blend toward white); the rest is unchanged.
        assert_eq!(spans[1].style.fg, Some(crate::config::hover_color(LINK_BLUE)));
        assert_eq!(spans[0].style, base);
        assert_eq!(spans[2].style, base);
    }
// ---- Round 34: lyrics edit mode ----

    /// A pause fixture whose lyrics live in a real temp-dir file (the edit
    /// session writes back to it).
    fn edit_fixture() -> (crate::ctx::Ctx, LyricsPane, std::path::PathBuf) {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "s2u-lyrics-pane-{}-{n}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("First.lrc");
        std::fs::write(
            &path,
            "[ti:First]\n[ar:Test Artist]\n# lrcgen-gap-align:v1\n\n[00:01.00]<00:01.20>hello <00:01.40>world\n[00:02.00]<00:02.10>a <00:02.30>b\n",
        )
        .unwrap();

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut first = song(1, "First");
        first.file = dir.join("First.flac").to_string_lossy().into_owned();
        ctx.queue = vec![first, song(2, "Second"), song(3, "Third")];
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        ctx.status.elapsed = Duration::from_millis(1500);

        let mut pane = LyricsPane::new(&ctx);
        pane.update_lyrics(&ctx).unwrap();
        assert!(pane.current_lyrics.is_some(), "lyrics loaded from the temp file");
        (ctx, pane, path)
    }

    fn action(action: crate::config::keys::CommonAction) -> ActionEvent {
        ActionEvent::from(std::sync::Arc::new(vec![Actions::Common(action)]))
    }

    #[test]
    fn edit_pencil_button_renders_left_and_toggles_edit_mode() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        // The buttons row only exists in lyrics mode, so check the pencil
        // while playing (lyrics visible, edit mode off -> `✎`).
        ctx.status.state = State::Play;
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        let row: Vec<char> = text.lines().nth(11).unwrap().chars().collect();
        assert_eq!(row[1], '✎', "pencil on the left of the button row: {text}");
        assert!(
            text.contains("● hide lyrics | ● fetch lyrics"),
            "right-aligned cluster intact: {text}"
        );
        assert_eq!(pane.edit_btn_area, Rect::new(1, 11, 1, 1), "click zone recorded");

        // A full press+release toggles edit mode ON: the glyph flips to
        // the active `✏` (persistent — no need to hold the button).
        let btn = pane.edit_btn_area;
        click_and_release(&mut pane, &ctx, btn);
        assert!(pane.edit_mode, "pencil click enters edit mode");
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert_eq!(text.lines().nth(11).unwrap().chars().nth(1), Some('✏'), "{text}");

        // Click again exits edit mode (nothing was edited, no write-back).
        let btn = pane.edit_btn_area;
        click_and_release(&mut pane, &ctx, btn);
        assert!(!pane.edit_mode, "pencil click leaves edit mode");
        assert!(pane.edit_session.is_none());
    }

    #[test]
    fn edit_mode_paused_keeps_the_lyrics_with_per_word_timings() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();

        let text = rendered(&mut pane, &mut ctx, 60, 12);
        // The lyrics stay visible (no track-details info panel).
        assert!(text.contains("hello"), "lyrics shown while paused in edit mode: {text}");
        assert!(text.contains("world"), "{text}");
        assert!(!text.contains("Title"), "no info panel while editing: {text}");
        // Each word carries its raw file timing, styled separately.
        assert!(text.contains("00:01.20"), "word timing shown: {text}");
        assert!(text.contains("00:01.40"), "{text}");
        assert!(
            pane.word_areas.iter().any(|w| w.line == 0 && w.word == 0),
            "word hit areas recorded: {:?}",
            pane.word_areas
        );
        // The selected word renders highlighted (bold); a non-selected
        // word keeps the plain lyrics style. The selection lands on the
        // word current at the pause position (1.5 s -> "world").
        let (_, sel_word) = pane.edit_selection.expect("selection after entering edit mode");
        assert_eq!(sel_word, 1, "the word current at the pause position is selected");
        let sel_rect = pane
            .word_areas
            .iter()
            .find(|w| w.line == 0 && w.word == sel_word)
            .unwrap()
            .rect;
        let selected = cell_style(&mut pane, &mut ctx, 60, 12, sel_rect.x, sel_rect.y);
        assert!(
            selected.add_modifier.contains(Modifier::BOLD),
            "selected word is bold: {selected:?}"
        );
        let plain = cell_style(&mut pane, &mut ctx, 60, 12, 2, 5);
        assert!(
            !plain.add_modifier.contains(Modifier::BOLD),
            "non-selected word is not bold: {plain:?}"
        );
    }

    #[test]
    fn edit_mode_playing_keeps_the_normal_karaoke_view() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        ctx.status.state = State::Play;
        pane.set_edit_mode(&ctx, true).unwrap();
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(!text.contains("00:01.20"), "no per-word timings while playing: {text}");
        assert!(text.contains("hello world"), "normal karaoke text: {text}");
        assert!(pane.word_areas.is_empty(), "no click targets while playing");
    }

    #[test]
    fn paused_without_edit_mode_still_shows_the_info_panel() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        let text = rendered(&mut pane, &mut ctx, 60, 12);
        assert!(text.contains("Title"), "paused + no edit mode shows info: {text}");
        assert!(!text.contains("hello"), "no lyrics: {text}");
    }

    #[test]
    fn clicking_a_word_selects_it() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        rendered(&mut pane, &mut ctx, 60, 12);
        let rect = pane.word_areas.iter().find(|w| w.line == 0 && w.word == 1).unwrap().rect;
        click(&mut pane, &ctx, rect);
        assert_eq!(pane.edit_selection, Some((0, 1)), "click selects the word");
    }

    #[test]
    fn word_navigation_moves_across_words_and_lines() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        // The anchor line is line 0 (elapsed 1.5s); the initial selection
        // is the word current at the pause position ("world", 1.40s).
        assert_eq!(pane.edit_selection, Some((0, 1)));

        pane.handle_action(&mut action(CommonAction::Right), &mut ctx).unwrap();
        assert_eq!(
            pane.edit_selection,
            Some((1, 0)),
            "right past the last word wraps to the next line"
        );
        pane.handle_action(&mut action(CommonAction::Left), &mut ctx).unwrap();
        assert_eq!(pane.edit_selection, Some((0, 1)), "left wraps back");
        pane.handle_action(&mut action(CommonAction::Down), &mut ctx).unwrap();
        assert_eq!(pane.edit_selection, Some((1, 1)), "down keeps the word column");
        pane.handle_action(&mut action(CommonAction::Up), &mut ctx).unwrap();
        assert_eq!(pane.edit_selection, Some((0, 1)), "up returns");
    }

    #[test]
    fn ctrl_s_saves_without_exiting() {
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 0));

        pane.handle_action(&mut action(CommonAction::LyricsNudgeUp), &mut ctx).unwrap();
        assert!(pane.edit_session.as_ref().unwrap().is_dirty());

        // `<C-s>` writes the change and stays in edit mode.
        pane.handle_action(&mut action(CommonAction::LyricsSave), &mut ctx).unwrap();
        assert!(pane.edit_mode, "ctrl+s keeps edit mode on");
        assert!(!pane.edit_session.as_ref().unwrap().is_dirty(), "pending edits saved");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(on_disk.contains("<00:01.21>hello"), "marker written: {on_disk}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn ctrl_c_saves_and_exits() {
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 0));

        pane.handle_action(&mut action(CommonAction::LyricsNudgeUp), &mut ctx).unwrap();

        // `<C-c>` writes the change and leaves edit mode.
        pane.handle_action(&mut action(CommonAction::LyricsSaveAndExit), &mut ctx).unwrap();
        assert!(!pane.edit_mode, "ctrl+c exits edit mode");
        assert!(!ctx.lyrics_edit_mode.get(), "legend flag cleared");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("<00:01.21>hello") && on_disk.contains("# lrcgen-gap-align:v1"),
            "saved with the stamp intact: {on_disk}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn esc_discards_pending_edits_without_saving() {
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 0));

        pane.handle_action(&mut action(CommonAction::LyricsNudgeUp), &mut ctx).unwrap();
        assert!(pane.edit_session.as_ref().unwrap().is_dirty());

        // Esc leaves edit mode WITHOUT writing the pending change; the
        // key is consumed so the settings panel does not open.
        let mut ev = action(CommonAction::Close);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(!pane.edit_mode, "esc exits edit mode");
        assert!(ev.is_consumed(), "esc consumed so settings does not open");
        assert!(pane.edit_session.is_none(), "edit session dropped");
        assert!(!ctx.lyrics_edit_mode.get(), "legend flag cleared");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            !on_disk.contains("<00:01.21>"),
            "discarded edit NOT written: {on_disk}"
        );
        assert!(on_disk.contains("<00:01.20>hello"), "file unchanged: {on_disk}");
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn esc_while_playing_exits_edit_mode_without_opening_settings() {
        // Round 38 regression: edit mode is toggleable in any play state,
        // but the old paused-only gate let Esc fall through to the global
        // ShowSettings binding while the song is playing. Esc must leave
        // edit mode (discard) and consume the key in ANY play state.
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        ctx.status.state = State::Play;
        assert!(pane.edit_mode, "edit mode is on while playing");

        let mut ev = action(CommonAction::Close);
        pane.handle_action(&mut ev, &mut ctx).unwrap();

        assert!(!pane.edit_mode, "esc exits edit mode while playing");
        assert!(ev.is_consumed(), "esc consumed so settings does not open");
        assert!(pane.edit_session.is_none(), "edit session dropped");
        assert!(!ctx.lyrics_edit_mode.get(), "legend flag cleared");
    }

    #[test]
    fn ctrl_c_saves_and_exits_while_playing() {
        // Round 38 audit of the same play-state hole: `<C-c>` (save +
        // exit) must also work while the song is playing, not only paused.
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 0));
        pane.handle_action(&mut action(CommonAction::LyricsNudgeUp), &mut ctx).unwrap();
        assert!(pane.edit_session.as_ref().unwrap().is_dirty());

        ctx.status.state = State::Play;
        pane.handle_action(&mut action(CommonAction::LyricsSaveAndExit), &mut ctx).unwrap();

        assert!(!pane.edit_mode, "ctrl+c exits edit mode while playing");
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("<00:01.21>hello") && on_disk.contains("# lrcgen-gap-align:v1"),
            "saved while playing, stamp intact: {on_disk}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn edit_mode_playing_does_not_claim_non_exit_keys() {
        // While playing, the edit key set beyond Esc/Ctrl+C/Ctrl+S stays
        // paused-gated: a nudge must neither move the selection nor be
        // consumed (the key falls through to the global handlers, exactly
        // as before round 38).
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        let selection = pane.edit_selection;
        ctx.status.state = State::Play;

        let mut ev = action(CommonAction::LyricsNudgeUp);
        pane.handle_action(&mut ev, &mut ctx).unwrap();

        assert!(pane.edit_mode, "still in edit mode");
        assert!(!ev.is_consumed(), "nudge key not consumed while playing");
        assert_eq!(pane.edit_selection, selection, "selection unchanged while playing");
    }

    #[test]
    fn exact_time_confirm_opens_the_value_popup() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        let (tx, rx) = crossbeam::channel::unbounded();
        ctx.app_event_sender = tx;
        pane.handle_action(&mut action(CommonAction::Confirm), &mut ctx).unwrap();
        let received = rx.try_recv().unwrap();
        assert!(
            matches!(
                received,
                crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))
            ),
            "Confirm opens the input modal: {received:?}"
        );
    }

    #[test]
    fn edit_mode_toggles_the_shared_cava_flag() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        assert!(!ctx.lyrics_edit_mode.get(), "flag off by default");
        pane.set_edit_mode(&ctx, true).unwrap();
        assert!(ctx.lyrics_edit_mode.get(), "entering edit mode sets the flag");
        pane.set_edit_mode(&ctx, false).unwrap();
        assert!(!ctx.lyrics_edit_mode.get(), "leaving edit mode clears the flag");
    }

    #[test]
    fn d_deletes_the_selected_word_not_the_whole_line() {
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        assert_eq!(
            pane.edit_selection,
            Some((0, 1)),
            "selection on the current lyric's word (world, at 1.5s)"
        );

        pane.handle_action(&mut action(CommonAction::LyricsDeleteWord), &mut ctx).unwrap();

        let session = pane.edit_session.as_ref().unwrap();
        assert_eq!(session.lines.len(), 2, "the LINE stays — only the word is deleted");
        assert_eq!(session.lines[0].content, "hello");
        assert_eq!(session.lines[1].content, "a b", "the next line is untouched");
        assert_eq!(pane.edit_selection, Some((0, 0)), "selection moves to the remaining word");
        // `<C-c>` saves the deletion and exits (the file keeps the header
        // + stamp, the line survives without the word).
        pane.handle_action(&mut action(CommonAction::LyricsSaveAndExit), &mut ctx).unwrap();
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(
            on_disk.contains("# lrcgen-gap-align:v1") && on_disk.contains("[00:01.00]<00:01.20>hello"),
            "word deleted in place, line + header/stamp intact: {on_disk}"
        );
        assert!(!on_disk.contains("world"), "deleted word gone: {on_disk}");
        assert!(
            on_disk.contains("[00:02.00]<00:02.10>a <00:02.30>b"),
            "next line untouched: {on_disk}"
        );
        std::fs::remove_dir_all(path.parent().unwrap()).ok();
    }

    #[test]
    fn e_opens_the_line_text_modal() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        let (tx, rx) = crossbeam::channel::unbounded();
        ctx.app_event_sender = tx;
        pane.handle_action(&mut action(CommonAction::LyricsEditLine), &mut ctx).unwrap();
        let received = rx.try_recv().unwrap();
        assert!(
            matches!(received, crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))),
            "e opens the text modal: {received:?}"
        );
    }

    #[test]
    fn i_and_a_open_the_insert_word_modals() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 1));
        for act in [CommonAction::LyricsInsertBefore, CommonAction::LyricsInsertAfter] {
            let (tx, rx) = crossbeam::channel::unbounded();
            ctx.app_event_sender = tx;
            pane.handle_action(&mut action(act), &mut ctx).unwrap();
            let received = rx.try_recv().unwrap();
            assert!(
                matches!(received, crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))),
                "insert-word modal opens: {received:?}"
            );
        }
    }

    #[test]
    fn insert_word_writes_into_the_same_line() {
        let (mut ctx, mut pane, path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        pane.edit_selection = Some((0, 1));
        // Drive the modal confirm directly: read the file, insert "there"
        // before word 1 (world), save — the modal's on_confirm path.
        let raw = std::fs::read_to_string(&path).unwrap();
        let mut session = super::LrcEditSession::open(path.clone(), raw);
        let idx = session.insert_word_at(0, 1, false, "there").unwrap();
        assert_eq!(idx, 1);
        session.save().unwrap();
        let saved = std::fs::read_to_string(&path).unwrap();
        assert!(
            saved.contains("[00:01.00]<00:01.20>hello <00:01.30>there <00:01.40>world"),
            "the word joins the SAME line: {saved}"
        );
        assert!(
            saved.contains("[00:02.00]<00:02.10>a <00:02.30>b"),
            "the next line is untouched: {saved}"
        );
    }

    #[test]
    fn o_and_O_open_the_add_line_modals() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        for act in [CommonAction::LyricsAddLineBefore, CommonAction::LyricsAddLineAfter] {
            let (tx, rx) = crossbeam::channel::unbounded();
            ctx.app_event_sender = tx;
            pane.handle_action(&mut action(act), &mut ctx).unwrap();
            let received = rx.try_recv().unwrap();
            assert!(
                matches!(received, crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))),
                "add-line modal opens: {received:?}"
            );
        }
    }

    #[test]
    fn t_opens_the_line_time_modal() {
        let (mut ctx, mut pane, _path) = edit_fixture();
        pane.set_edit_mode(&ctx, true).unwrap();
        let (tx, rx) = crossbeam::channel::unbounded();
        ctx.app_event_sender = tx;
        pane.handle_action(&mut action(CommonAction::LyricsLineTime), &mut ctx).unwrap();
        let received = rx.try_recv().unwrap();
        assert!(
            matches!(received, crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))),
            "t opens the line-time modal: {received:?}"
        );
    }

}


#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod schedule_tests {
    use super::{LyricsPane, Pane};
    use crate::{tests::fixtures::ctx, ui::UiEvent};

    #[test]
    fn playback_state_change_rearms_the_next_line_schedule() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = LyricsPane::new(&ctx);
        // Pretend a schedule for the next line was already armed.
        pane.last_requested_line_idx = 5;

        pane.on_event(&mut UiEvent::PlaybackStateChanged, true, &ctx).unwrap();

        assert_eq!(
            pane.last_requested_line_idx,
            usize::MAX,
            "pause/resume invalidates the stale next-line schedule so the \
             "
        );
    }

    #[test]
    fn pausing_reanchors_the_edit_selection_to_the_current_lyric() {
        use crate::mpd::commands::State;
        use std::time::Duration;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = LyricsPane::new(&ctx);
        let dir =
            std::env::temp_dir().join(format!("s2u-lyrics-anchor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.lrc");
        std::fs::write(
            &path,
            "[00:01.00]<00:01.10>one <00:01.30>two\n[00:03.00]<00:03.10>three\n",
        )
        .unwrap();
        let mut song = crate::mpd::commands::Song::default();
        song.id = 1;
        song.file = dir.join("song.flac").to_string_lossy().into_owned();
        song.metadata.insert("title".to_string(), "Song".into());
        song.metadata.insert("artist".to_string(), "Test Artist".into());
        ctx.queue = vec![song];
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        ctx.status.elapsed = Duration::from_millis(1500);
        pane.update_lyrics(&ctx).unwrap();
        assert!(pane.current_lyrics.is_some(), "lyrics loaded from the sidecar");
        pane.set_edit_mode(&ctx, true).unwrap();
        assert!(pane.edit_mode, "edit mode enabled");
        // Move the selection off the current lyric (line 0) to line 1.
        pane.edit_selection = Some((1, 0));
        assert_eq!(pane.anchor_line(&ctx), 0, "anchor = the lyric at the pause position");

        pane.on_event(&mut UiEvent::PlaybackStateChanged, true, &ctx).unwrap();

        assert_eq!(
            pane.edit_selection,
            Some((0, 1)),
            "pausing re-anchors the selection to the word current at the pause position"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn pausing_keeps_the_selection_when_already_on_the_current_lyric() {
        use crate::mpd::commands::State;
        use std::time::Duration;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = LyricsPane::new(&ctx);
        let dir =
            std::env::temp_dir().join(format!("s2u-lyrics-anchor2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("song.lrc");
        std::fs::write(
            &path,
            "[00:01.00]<00:01.10>one <00:01.30>two\n[00:03.00]<00:03.10>three\n",
        )
        .unwrap();
        let mut song = crate::mpd::commands::Song::default();
        song.id = 1;
        song.file = dir.join("song.flac").to_string_lossy().into_owned();
        song.metadata.insert("title".to_string(), "Song".into());
        song.metadata.insert("artist".to_string(), "Test Artist".into());
        ctx.queue = vec![song];
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        ctx.status.elapsed = Duration::from_millis(1500);
        pane.update_lyrics(&ctx).unwrap();
        pane.set_edit_mode(&ctx, true).unwrap();
        // Already on the current lyric, second word — a pause keeps the
        // word the user is editing.
        pane.edit_selection = Some((0, 1));

        pane.on_event(&mut UiEvent::PlaybackStateChanged, true, &ctx).unwrap();

        assert_eq!(
            pane.edit_selection,
            Some((0, 1)),
            "already on the anchor: the word selection is preserved"
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
