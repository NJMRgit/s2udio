use super::*;

/// One search-result row: the item name (bold-ish white) with a dim
/// sublabel of its type (+ the series for an episode), so the match kind is
/// clear at a glance. Only movies, series and episodes are listed
/// (`crate::jellyfin::is_search_kind`), so the type name is the label.
pub(super) fn search_result_rows(items: &[JfItem]) -> Vec<ListItem<'static>> {
    items
        .iter()
        .map(|item| {
            let mut parts: Vec<String> = vec![item.kind.clone()];
            if item.kind == "Episode"
                && let Some(series) = &item.series_name
            {
                parts.push(series.clone());
            }
            let mut line = Line::from(Span::styled(
                item.name.clone(),
                ratatui::style::Style::default().fg(ratatui::style::Color::White),
            ));
            line.push_span(Span::styled(
                format!("  — {}", parts.join(" · ")),
                ratatui::style::Style::default().add_modifier(ratatui::style::Modifier::DIM),
            ));
            ListItem::from(line)
        })
        .collect()
}

/// A season/episode hint parsed out of the search query: `S02`, `S02E03`,
/// `season 2` or `2x03`. It is applied when the jump reaches a series or
/// season result, so "Show season 2" opens on season 2 instead of on the
/// show's first season.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct JfSearchHint {
    pub(super) season: Option<i32>,
    pub(super) episode: Option<i32>,
}

/// Split a search query into the free text sent to `SearchHints` and the
/// season/episode hint. `("overgeared S01E03")` → `("overgeared", S01E03)`.
/// A query that is nothing but a hint keeps its full text for the server
/// (there is still something to search for), only losing the hint token.
pub(super) fn split_query_hint(query: &str) -> (String, JfSearchHint) {
    let mut hint = JfSearchHint::default();
    let mut text: Vec<&str> = Vec::new();
    let tokens: Vec<&str> = query.split_whitespace().collect();
    let mut idx = 0;
    while idx < tokens.len() {
        let raw = tokens[idx];
        let token = raw.trim_matches(|c: char| !c.is_ascii_alphanumeric());
        if hint.season.is_none() {
            if let Some((season, episode)) = parse_season_episode_token(token) {
                hint.season = Some(season);
                hint.episode = episode;
                idx += 1;
                continue;
            }
            if token.eq_ignore_ascii_case("season")
                && let Some(next) = tokens.get(idx + 1)
            {
                let next = next.trim_matches(|c: char| !c.is_ascii_alphanumeric());
                if let Ok(season) = next.parse::<i32>() {
                    hint.season = Some(season);
                    idx += 2;
                    continue;
                }
            }
            let lower = token.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("season")
                && !rest.is_empty()
                && rest.chars().all(|c| c.is_ascii_digit())
                && let Ok(season) = rest.parse::<i32>()
            {
                hint.season = Some(season);
                idx += 1;
                continue;
            }
        }
        text.push(raw);
        idx += 1;
    }
    let text = text.join(" ");
    let text = if text.trim().is_empty() {
        query.trim().to_owned()
    } else {
        text
    };
    (text, hint)
}

/// `S03E07` / `s3` / `3x07` (case-insensitive) → (season, episode).
fn parse_season_episode_token(token: &str) -> Option<(i32, Option<i32>)> {
    let lower = token.to_ascii_lowercase();
    let all_digits = |value: &str| !value.is_empty() && value.chars().all(|c| c.is_ascii_digit());
    if let Some(rest) = lower.strip_prefix('s')
        && rest.chars().next().is_some_and(|c| c.is_ascii_digit())
    {
        if let Some((season, episode)) = rest.split_once('e') {
            if all_digits(season) && all_digits(episode) {
                return Some((season.parse().ok()?, Some(episode.parse().ok()?)));
            }
        } else if all_digits(rest) {
            return Some((rest.parse().ok()?, None));
        }
    }
    if let Some((season, episode)) = lower.split_once('x')
        && all_digits(season)
        && all_digits(episode)
    {
        return Some((season.parse().ok()?, Some(episode.parse().ok()?)));
    }
    None
}

impl JellyfinPane {
    /// The selected search result, if any.
    pub(super) fn search_selected(&self) -> Option<&JfItem> {
        self.search_results_state
            .selected()
            .and_then(|idx| self.search_results.get(idx))
    }

    /// Cycle Home → Libraries → Search (Shift+Tab / toggle click). The
    /// search state stays for the session.
    ///
    /// Round 73.2: the flip no longer focuses the query input (the `S` key
    /// does that) — it always RELEASES it instead. Releasing is required,
    /// not just optional: `search_input_focused` drives this pane's
    /// keyboard phase (Up/Down move the results only while the input is
    /// NOT focused), so a flip that left the flag true without insert mode
    /// would freeze the results list.
    pub(super) fn toggle_mode(&mut self, ctx: &mut Ctx) -> Result<()> {
        let next = self.mode.next();
        // Entering the search page from Home/Libraries remembers the page
        // Esc returns to.
        if next == JellyfinTabMode::Search && self.mode != JellyfinTabMode::Search {
            self.search_return = self.mode;
        }
        if self.mode == JellyfinTabMode::Home && next != JellyfinTabMode::Home {
            self.wall.hide(ctx);
        }
        self.mode = next;
        self.release_search_input(ctx);
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 73.2 (revision B: `S`): the target of the search key — show the
    /// search page with the query input focused (the caret in the field).
    /// The mode is SET, not toggled, so the key always lands on Search;
    /// nothing is cleared, so the session query and its results are still
    /// there when the page returns.
    pub(super) fn jump_to_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        if self.mode != JellyfinTabMode::Search {
            self.search_return = self.mode;
        }
        if self.mode == JellyfinTabMode::Home {
            self.wall.hide(ctx);
        }
        self.mode = JellyfinTabMode::Search;
        self.focus_search_input(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 62.1 (S1/S2 parity): leave the search page back to the
    /// Jellyfin browser — clears the query + results and returns to
    /// keyboard navigation.
    pub(super) fn exit_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = self.search_return;
        self.release_search_input(ctx);
        self.search_results_state.select(None);
        self.search_results.clear();
        ctx.input.clear_buffer(self.search_buffer);
        self.sync_poster(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Focus the search input row: the buffer becomes the active
    /// insert-mode buffer so printable keys type into the query (round
    /// 60b D4 — the buffer was never activated before, so typing reached
    /// no handler and `SearchHints` never fired).
    pub(super) fn focus_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = true;
        self.search_left_presses = 0;
        ctx.input.insert_mode(self.search_buffer);
    }
    /// Leave the input (results / mode toggle / tab switch): drop insert
    /// mode when this pane's buffer is the active one — the input manager
    /// is global, so an unattended insert buffer would swallow keys meant
    /// for other panes, tabs and modals.
    pub(super) fn release_search_input(&mut self, ctx: &Ctx) {
        self.search_input_focused = false;
        if ctx.input.is_active(self.search_buffer) {
            ctx.input.normal_mode();
        }
    }

    /// Fire a server-side search for the current query (deduped; an empty
    /// query clears the results). Results arrive as `JF_SEARCH`.
    pub(super) fn run_search(&mut self, ctx: &Ctx) {
        let raw = ctx.input.value(self.search_buffer).trim().to_owned();
        if raw.is_empty() {
            self.search_results.clear();
            self.search_results_state = ListState::default();
            let _ = ctx.render();
            return;
        }
        if self.search_pending {
            return;
        }
        if self.load_server(ctx).is_none() {
            return;
        }
        // The season/episode hint is ours, not the server's: "overgeared
        // S01E03" searches "overgeared" and the hint opens the episode.
        let (query, _) = split_query_hint(&raw);
        self.search_pending = true;
        self.last_sent_query = raw;
        self.request_fetch(ctx, WorkRequest::FetchJellyfinSearch { query });
    }

    /// Enter/`d`/`→`/double-click on a result: **go to the item in the
    /// library tree**. Every result leads to a selected item there — a movie
    /// or a specific episode starts playing once the tree is on it, and a
    /// partly watched one asks Continue / Start over **before** the video
    /// starts (the jump plays the tree's copy of the item, so its saved
    /// position is known up front; see [`JellyfinPane::start_jump`]). A series
    /// only opens (the cursor lands inside it, on the hinted season/episode
    /// when the query named one).
    pub(super) fn search_activate(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.search_selected().cloned() else { return Ok(()) };
        let (_, hint) = split_query_hint(&ctx.input.value(self.search_buffer));
        let play = matches!(item.kind.as_str(), "Movie" | "Episode");
        self.start_jump(item, play, hint.season, hint.episode, ctx)
    }

    /// Search-mode keys (parity with the MPD/playlists search): `d`/`→`
    /// move from the input into the results, `a`/`←` return, Enter opens
    /// the options menu, Esc clears nothing (results are not markable
    /// here — the list keeps single selection).
    pub(super) fn handle_search_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if self.search_input_focused && !self.search_results.is_empty() {
                        self.release_search_input(ctx);
                        ctx.render()?;
                    } else if !self.search_input_focused {
                        self.search_activate(ctx)?;
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => {
                    if self.search_input_focused {
                        // Round 62.1 (S2): Left #2 from the search bar
                        // behaves exactly like Esc #2 — leave the search
                        // page back to the Jellyfin browser.
                        self.exit_search(ctx)?;
                    } else {
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        if let Some(action) = event.claim_common() {
            match action {
                CommonAction::Up | CommonAction::Down if !self.search_input_focused => {
                    if self.search_results.is_empty() {
                        return Ok(());
                    }
                    let len = self.search_results.len();
                    let sel = self.search_results_state.selected().unwrap_or(0);
                    let next = if matches!(action, CommonAction::Up) {
                        sel.saturating_sub(1)
                    } else {
                        (sel + 1).min(len - 1)
                    };
                    self.search_results_state.select(Some(next));
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Right if !self.search_results.is_empty() => {
                    self.release_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Left if !self.search_input_focused => {
                    self.focus_search_input(ctx);
                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::Confirm => {
                    // Enter is "go to it": the tree opens on the result
                    // (and a movie/episode plays there). The item menu stays
                    // on the context-menu key / right-click.
                    return self.search_activate(ctx);
                }
                CommonAction::ContextMenu => {
                    return self.search_context_menu(ctx);
                }
                CommonAction::Close => {
                    // Round 62.1 (S1/S2): staged search exit — Esc #1 from
                    // results goes back to the search bar, Esc #2 from the
                    // bar leaves the search page; both are consumed so the
                    // app-level ShowSettings half never fires.
                    if self.search_input_focused {
                        self.exit_search(ctx)?;
                    } else {
                        self.focus_search_input(ctx);
                        ctx.render()?;
                    }
                    event.consume();
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }
        Ok(())
    }

    /// Search-mode mouse: clicks on the input row focus it; clicks on the
    /// results select; double-click plays; right-click opens the menu;
    /// the wheel moves the selection.
    pub(super) fn handle_search_mouse(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position: ratatui::layout::Position = event.into();
        if matches!(event.kind, MouseEventKind::LeftClick)
            && self.search_results_area.y > 0
            && self.toggle_areas[0].y > 0
            && position.y > self.toggle_areas[0].y
            && position.y < self.search_results_area.y
        {
            self.focus_search_input(ctx);
            ctx.render()?;
            return Ok(());
        }
        let area = self.search_results_area;
        if !area.contains(position) {
            return Ok(());
        }
        let row = usize::from(event.y.saturating_sub(area.y));
        match event.kind {
            MouseEventKind::LeftClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.release_search_input(ctx);
                    ctx.render()?;
                }
            }
            MouseEventKind::DoubleClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.search_activate(ctx)?;
                }
            }
            MouseEventKind::RightClick => {
                if row < self.search_results.len() {
                    self.search_results_state.select(Some(row));
                    self.release_search_input(ctx);
                    return self.search_context_menu(ctx);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                if self.search_results.is_empty() {
                    return Ok(());
                }
                let len = self.search_results.len();
                let sel = self.search_results_state.selected().unwrap_or(0);
                let next = if matches!(event.kind, MouseEventKind::ScrollUp) {
                    sel.saturating_sub(1)
                } else {
                    (sel + 1).min(len - 1)
                };
                self.search_results_state.select(Some(next));
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }
}

