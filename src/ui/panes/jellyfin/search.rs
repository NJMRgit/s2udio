use super::*;

/// One search-result row: the item name (bold-ish white) with a dim
/// sublabel of its type + artist/album/series, so the match kind is
/// clear at a glance.
pub(super) fn search_result_rows(items: &[JfItem]) -> Vec<ListItem<'static>> {
    items
        .iter()
        .map(|item| {
            let mut parts: Vec<String> = Vec::new();
            parts.push(match item.kind.as_str() {
                "Audio" => "Song".to_owned(),
                "MusicAlbum" => "Album".to_owned(),
                "MusicArtist" => "Artist".to_owned(),
                "Movie" => "Movie".to_owned(),
                "Episode" => "Episode".to_owned(),
                "Series" => "Series".to_owned(),
                other => other.to_owned(),
            });
            match item.kind.as_str() {
                "Audio" => {
                    if let Some(artist) = &item.artist {
                        parts.push(artist.clone());
                    }
                    if let Some(album) = &item.album {
                        parts.push(album.clone());
                    }
                }
                "Episode" => {
                    if let Some(series) = &item.series_name {
                        parts.push(series.clone());
                    }
                }
                _ => {}
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

impl JellyfinPane {
    /// The selected search result, if any.
    pub(super) fn search_selected(&self) -> Option<&JfItem> {
        self.search_results_state
            .selected()
            .and_then(|idx| self.search_results.get(idx))
    }

    /// Flip Libraries <-> Search (Shift+Tab / toggle click). The search
    /// state stays for the session.
    ///
    /// Round 73.2: the flip no longer focuses the query input (the `S` key
    /// does that) — it always RELEASES it instead. Releasing is required,
    /// not just optional: `search_input_focused` drives this pane's
    /// keyboard phase (Up/Down move the results only while the input is
    /// NOT focused), so a flip that left the flag true without insert mode
    /// would freeze the results list.
    pub(super) fn toggle_mode(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = match self.mode {
            JellyfinTabMode::Libraries => JellyfinTabMode::Search,
            JellyfinTabMode::Search => JellyfinTabMode::Libraries,
        };
        self.release_search_input(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 73.2 (revision B: `S`): the target of the search key — show the
    /// search page with the query input focused (the caret in the field).
    /// The mode is SET, not toggled, so the key always lands on Search;
    /// nothing is cleared, so the session query and its results are still
    /// there when the page returns.
    pub(super) fn jump_to_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = JellyfinTabMode::Search;
        self.focus_search_input(ctx);
        ctx.render()?;
        Ok(())
    }
    /// Round 62.1 (S1/S2 parity): leave the search page back to the
    /// Jellyfin browser — clears the query + results and returns to
    /// keyboard navigation.
    pub(super) fn exit_search(&mut self, ctx: &mut Ctx) -> Result<()> {
        self.mode = JellyfinTabMode::Libraries;
        self.release_search_input(ctx);
        self.search_results_state.select(None);
        self.search_results.clear();
        ctx.input.clear_buffer(self.search_buffer);
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
        let query = ctx.input.value(self.search_buffer).trim().to_owned();
        if query.is_empty() {
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
        self.search_pending = true;
        self.last_sent_query = query.clone();
        self.request_fetch(ctx, WorkRequest::FetchJellyfinSearch { query });
    }

    /// `d`/`→`/double-click on a result: play it with the existing
    /// Jellyfin behavior (audio via MPD temp stream, video via mpv).
    fn search_activate(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.search_selected().cloned() else { return Ok(()) };
        if !item.is_playable() {
            // Containers just select (their subtree is browsable in
            // Libraries mode); nothing to play.
            return Ok(());
        }
        let Some(server) = self.load_server(ctx) else {
            return Ok(());
        };
        if item.is_audio() {
            let url = server.stream_url(&item.id);
            self.play_temp_url(
                ctx,
                JF_PLAY,
                PaneType::Jellyfin {
                    tree: TreeBrowserArgs::default(),
                },
                item.name.clone(),
                url,
            );
            status_info!("Playing {}", item.name);
        } else {
            let url = server.video_stream_url(&item.id);
            Self::play_video(
                ctx,
                url,
                item.name.clone(),
                item.id.clone(),
                item.season_id.clone(),
            )?;
        }
        Ok(())
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
                    return self.search_context_menu(ctx);
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

