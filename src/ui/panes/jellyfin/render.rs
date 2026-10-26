use super::*;
use super::search::search_result_rows;

impl JellyfinPane {
    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        if area.height < 2 {
            return Ok(());
        }
        let [toggle_area, content] = Layout::vertical([
                Constraint::Length(1),
                Constraint::Min(0),
            ])
            .areas(area);
        self.render_toggle(frame, toggle_area, ctx);
        match self.mode {
            JellyfinTabMode::Home => self.render_home(frame, content, ctx),
            JellyfinTabMode::Libraries => {
                self.render_tree_browser(frame, content, ctx)
            }
            JellyfinTabMode::Search => self.render_search(frame, content, ctx),
        }
    }
    /// Drop the temporary play song once playback has moved on.
    /// Second (dim) line of an item row.
    fn item_subline(item: &JfItem) -> String {
        let mut parts: Vec<String> = Vec::new();
        match item.kind.as_str() {
            "MusicAlbum" => {
                if let Some(artist) = &item.album_artist {
                    parts.push(artist.clone());
                }
                if let Some(year) = item.year {
                    parts.push(year.to_string());
                }
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} tracks"));
                }
            }
            "MusicArtist" => {
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} albums"));
                }
            }
            "Audio" => {
                if let Some(artist) = &item.artist {
                    parts.push(artist.clone());
                }
                if let Some(album) = &item.album {
                    parts.push(album.clone());
                }
                if let Some(secs) = item.runtime_secs {
                    parts.push(format!("{}:{:02}", secs / 60, secs % 60));
                }
            }
            "Movie" | "Episode" | "Video" => {
                if let Some(year) = item.year {
                    parts.push(year.to_string());
                }
                if let Some(secs) = item.runtime_secs {
                    parts.push(format!("{}:{:02}", secs / 60, secs % 60));
                }
                let mut lang_parts: Vec<String> = Vec::new();
                if !item.audio_languages.is_empty() {
                    lang_parts.push(format!("a: {}", item.audio_languages.join(",")));
                }
                if !item.subtitle_languages.is_empty() {
                    lang_parts.push(format!("s: {}", item.subtitle_languages.join(",")));
                }
                if !lang_parts.is_empty() {
                    parts.push(lang_parts.join(" "));
                }
            }
            _ => {
                if let Some(count) = item.child_count {
                    parts.push(format!("{count} items"));
                }
            }
        }
        // Where the server says playback stopped: the same percentage and
        // "x left" the web UI's cards show, so a partially watched item
        // reads as such here.
        if let Some(watched) = item.progress_fraction() {
            parts.push(format!("{}%", (watched * 100.0).round() as u32));
        }
        if let Some(resume) = item.resume_secs() {
            let left = item
                .runtime_secs
                .map_or(0, |runtime| runtime.saturating_sub(resume));
            if left > 0 {
                parts.push(format!("{} left", super::home::clock(left)));
            }
        }
        parts.join(" · ")
    }
pub(super) fn render_toggle(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        self.toggle_areas = [Rect::default(); 2];
        self.back_area = Rect::default();
        if area.height == 0 {
            return;
        }
        // The library tree is a sub-page of Home, so only two segments are
        // left; the tree page shows Home as active.
        let segments = [
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Home",
                active: self.mode != JellyfinTabMode::Search,
            },
            crate::ui::widgets::sub_tab_bar::Segment {
                label: "Search",
                active: self.mode == JellyfinTabMode::Search,
            },
        ];
        let x = area.x.saturating_add(1);
        let bar = crate::ui::widgets::sub_tab_bar::SubTabBar::new(
            &segments,
            x,
            area.y,
            area.right().saturating_sub(1),
        );
        let areas = bar.render(frame, ctx);
        for (idx, seg_area) in areas.into_iter().take(2).enumerate() {
            self.toggle_areas[idx] = seg_area;
        }
        // Round 60 (B3): `↰ Back` at the right end of the row, visible
        // only inside a folder/collection (hidden at the libraries root
        // and in search mode).
        let visible = self.mode == JellyfinTabMode::Libraries && self.selected.is_some();
        self.back_area = crate::ui::draw_back_button(frame, area, visible, ctx);
        // The tree page sits under Home now: a `⌂ Home` button returns to the
        // carousels (to the left of Back, or at the row's end without it).
        self.home_button_area = crate::ui::draw_home_button(
            frame,
            area,
            self.mode == JellyfinTabMode::Libraries,
            (self.back_area.width > 0).then_some(self.back_area.x),
            ctx,
        );
        self.draw_loading(frame, area, ctx);
    }
    /// Round 92: the animated loading indicator, right-aligned on the
    /// mode-toggle row (`⭘ Libraries ● Search`). It ends one blank column
    /// left of `↰ Back` when that button is visible, degrades to the spinner
    /// glyph alone when the label does not fit, and draws nothing at all
    /// rather than overwriting a mode label. The redraw tick that keeps the
    /// spinner moving is re-armed here, so the animation costs nothing while
    /// no fetch is in flight.
    fn draw_loading(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let Some(since) = self.loading_since else { return };
        if self.fetches_in_flight == 0 || area.height == 0 {
            return;
        }
        // A tracked fetch that never reports back (dropped result, hung work
        // thread) must not leave the indicator spinning forever.
        if self.fetch_activity_at.elapsed() > LOADING_WATCHDOG {
            log::warn!(
                in_flight = self.fetches_in_flight; "Jellyfin fetch never reported back; clearing the loading indicator"
            );
            self.fetches_in_flight = 0;
            self.loading_since = None;
            if let Some(id) = self.loading_tick.take() {
                ctx.scheduler.cancel(id);
            }
            return;
        }
        self.schedule_loading_tick(ctx);
        let frame_idx = (since.elapsed().as_millis() as u64 / LOADING_FRAME_MS) as usize
            % LOADING_FRAMES.len();
        let spinner = LOADING_FRAMES[frame_idx];
        // The mode labels own the left end of the row; the indicator never
        // reaches into them.
        let labels_end = self.toggle_areas
            .iter()
            .map(|area| area.right())
            .max()
            .unwrap_or(area.x)
            .max(area.x);
        // The indicator ends left of whichever page button is on the row.
        let right = [self.back_area, self.home_button_area]
            .iter()
            .filter(|rect| rect.width > 0)
            .map(|rect| rect.x.saturating_sub(1))
            .min()
            .unwrap_or(area.right().saturating_sub(1));
        let style = ctx.config.as_list_text_style();
        let full = format!("{spinner} {LOADING_LABEL}");
        let full_w =
            unicode_width::UnicodeWidthStr::width(full.as_str()).min(u16::MAX as usize) as u16;
        // One blank column of slack on either side of the indicator.
        let (text, width) = if right >= labels_end + full_w + 2 {
            (full, full_w)
        } else if right >= labels_end + 2 {
            (spinner.to_owned(), 1)
        } else {
            return;
        };
        let x = right.saturating_sub(width);
        if x <= labels_end {
            return;
        }
        frame.render_widget(
            ratatui::widgets::Paragraph::new(text).style(style),
            Rect { x, y: area.y, width, height: 1 },
        );
    }
    /// Search mode (round 60 B2): `Search:` input row + separator, the
    /// scrollable results list and the info box.
    pub(super) fn render_search(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> Result<()> {
        // Round 60c (S1): ONE combined frame (same shared layout as the
        // Playlists search) — `Search:` input row on top, connected
        // `├───…───┤` divider, results list in the same box, `Results`
        // on the bottom edge (`╰─Results───…──╯`). The Info Box stays
        // below.
        let [frame_area, info_area] = Layout::vertical([
                Constraint::Min(4),
                Constraint::Percentage(33),
            ])
            .areas(area);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title_bottom(ratatui::text::Line::from("─Results"));
        let inner = block.inner(frame_area);
        // Render the box first so the connected divider's `├`/`┤`
        // junctions can overwrite the box's border cells at that row.
        frame.render_widget(block, frame_area);
        let query = ctx.input.value(self.search_buffer);
        let content = crate::ui::render_search_frame_top(
            frame,
            inner,
            &query,
            self.search_input_focused,
            None,
            ctx,
        );
        let (list_area, scrollbar_area) = if ctx.config.theme.scrollbar.is_some() {
            crate::ui::scrollbar_strip(content)
        } else {
            (content, Rect::default())
        };
        self.search_results_area = list_area;
        let list = List::new(search_result_rows(&self.search_results))
            .style(ctx.config.as_list_name_style())
            .highlight_style(
                if !self.search_input_focused {
                    ctx.config.theme.hovered_item_style
                } else {
                    ctx.config.theme.current_item_style
                },
            );
        ratatui::widgets::StatefulWidget::render(
            list,
            list_area,
            frame.buffer_mut(),
            &mut self.search_results_state,
        );
        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && scrollbar_area.width > 0
        {
            let max_offset =
                self.search_results.len().saturating_sub(list_area.height as usize);
            let position = self.search_results_state.offset().min(max_offset);
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position),
            );
        }
        self.render_search_info(frame, info_area, ctx);
        Ok(())
    }
}

impl JellyfinPane {
    /// Round 64: carve a poster shelf out of the items list's bottom
    /// (between the list and the Info box) when a poster-relevant item is
    /// selected and the terminal has room. The poster is drawn there as a
    /// compact overlay instead of filling a 40% column of the Info box
    /// (user feedback: info-box layout + poster/preview rework).
    ///
    /// Round 65: the Info box takes the SAME capped height as the other
    /// library pages (MPD/Playlists/Downloads: info_box_height(2/3) with
    /// the configured cap, default 15 rows) instead of a fixed 33% share,
    /// and the poster shelf is twice as tall (18 rows).
    pub(super) fn layout_vertical(&mut self, right: Rect) -> (Rect, Rect) {
        let info_h = self.tree_args.info_box_height(right.height * 2 / 3);
        let items_h = right.height.saturating_sub(info_h);
        let [items, info] = Layout::vertical([
            Constraint::Length(items_h),
            Constraint::Length(info_h),
        ])
        .areas(right);
        self.poster_shelf = None;
        // Round 64-2 (user): the preview is centered over the pane (was
        // right-aligned). Round 66: the shelf WIDTH follows the artwork —
        // the image always displays at the full inner height and the box
        // widens for wider images, centered, with a 1-char outline ring
        // each side.
        if self.poster_relevant() {
            let shelf_w = self
                .poster
                .natural_width_cells(POSTER_SHELF_H.saturating_sub(2))
                .saturating_add(4);
            // The shelf reserves artwork + 4 columns so the rendered image
            // (centered inside the shelf inset by one cell) keeps its exact
            // pre-round-67 size/position; the framing outline is gone, so
            // this is just empty padding around the borderless image.
            // The shelf is carved from the items area (never the Info box),
            // so only the items side needs room for it — the Info box keeps
            // the same 15-row cap as the other library pages even when the
            // poster is shown.
            let fits_horizontally = right.width >= shelf_w + POSTER_SHELF_RIGHT_MARGIN * 2;
            if fits_horizontally && items.height > POSTER_SHELF_H + 6 {
                self.poster_shelf = Some(Rect {
                    x: right.x + (right.width - shelf_w) / 2,
                    y: items.y + items.height - POSTER_SHELF_H,
                    width: shelf_w,
                    height: POSTER_SHELF_H,
                });
                let items_shrunk = Rect {
                    height: items.height - POSTER_SHELF_H,
                    ..items
                };
                return (items_shrunk, info);
            }
        }
        (items, info)
    }
    pub(super) fn item_row(&self, idx: usize, hovered: bool, ctx: &Ctx) -> ListItem<'static> {
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let playing_id = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.item_id.clone()
        } else {
            ctx.find_current_song_in_queue()
                .and_then(|(_, song)| crate::jellyfin::item_id_from_url(&song.file))
        };
        let item = &self.items[idx];
        let is_playing = playing_id.as_deref() == Some(item.id.as_str());
        let prefix = if is_playing {
            "▶ "
        } else if item.played {
            "✓ "
        } else if item.is_container() {
            "▸ "
        } else {
            "  "
        };
        let name_style = if is_playing {
            ctx.config.theme.current_item_style
        } else {
            base
        };
        let sub_style = if is_playing {
            ctx.config.theme.current_item_style
        } else {
            dim
        };
        let mut lines = vec![
            Line::from(Span::styled(format!("{prefix}{}", item.name), name_style)),
            Line::from(Span::styled(format!("  {}", Self::item_subline(item)),
            sub_style)),
        ];
        if hovered {
            for line in lines.iter_mut() {
                *line = line.clone().patch_style(ctx.config.theme.hovered_item_style);
            }
        }
        ListItem::new(lines)
    }
}

