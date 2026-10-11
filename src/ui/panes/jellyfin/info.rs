use super::*;

impl JellyfinPane {
    pub(super) fn render_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let key = ctx.config.theme.preview_label_style;
        let group = ctx.config.theme.preview_metadata_group_style;
        let bold = ratatui::style::Style::default().add_modifier(Modifier::BOLD);
        let white = ratatui::style::Style::default().fg(Color::White);
        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(
                ratatui::style::Style::default,
                |c| { ratatui::style::Style::default().fg(c) },
            );
        let dim = base.add_modifier(Modifier::DIM);
        let mut header_prefix: Option<String> = None;
        let mut header_title: Option<String> = None;
        let mut header_time = String::new();
        let mut header_episode_left: Vec<Span<'static>> = Vec::new();
        let mut header_episode_right: Vec<Span<'static>> = Vec::new();
        let mut header_desc = false;
        let mut rows: Vec<ListItem> = Vec::new();
        let mut credits: Vec<Line<'static>> = Vec::new();
        let selected = self.selected_item();
        let display_item: Option<JfItem> = selected
            .clone()
            .map(|item| {
                self.full_item.clone().filter(|f| f.id == item.id).unwrap_or(item)
            });
        let video_layout = display_item
            .as_ref()
            .is_some_and(|item| matches!(item.kind.as_str(), "Movie" | "Episode"));
        match display_item {
            Some(item) if video_layout => {
                let name = if item.kind == "Episode" {
                    item.series_name.clone().unwrap_or_else(|| item.name.clone())
                } else {
                    item.name.clone()
                };
                match item.year {
                    Some(year) => {
                        header_prefix = Some(format!("{year} -- "));
                        header_title = Some(name);
                    }
                    None => header_title = Some(name),
                }
                if let Some(secs) = item.runtime_secs {
                    header_time = format!(
                        "Time: {}", crate ::ui::panes::lyrics::format_clock(secs)
                    );
                }
                if item.kind == "Episode" {
                    header_episode_left.push(Span::styled("Episode: ", base));
                    header_episode_left.push(Span::styled(item.name.clone(), base));
                    if let (Some(season), Some(episode)) = (
                        item.season_number,
                        item.index_number,
                    ) {
                        header_episode_right
                            .push(
                                Span::styled(format!("S{season:02}E{episode:02}"), base),
                            );
                    }
                }
                header_desc = item
                    .overview
                    .as_deref()
                    .is_some_and(|d| !d.trim().is_empty()) || item.director.is_some()
                    || item.writer.is_some() || !item.starring.is_empty();
                if let Some(overview) = item
                    .overview
                    .as_deref()
                    .filter(|d| !d.trim().is_empty())
                {
                    // Round 64: full-width text (the poster column is gone),
                    // wrapped to fit the one-cell side margins.
                    let text_width = (area.width.saturating_sub(6)).max(10) as usize;
                    for line in crate::ui::widgets::wrap::wrap_to_width(
                        &crate::ui::panes::lyrics::scrub_emoji(overview),
                        text_width,
                    ) {
                        rows.push(
                            ListItem::new(
                                Line::from(
                                    Span::styled(line, ctx.config.as_list_text_style()),
                                ),
                            ),
                        );
                    }
                }
                if let Some(director) = &item.director {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Director: ", key), Span::styled(director
                                    .clone(), white),
                                ],
                            ),
                        );
                }
                if let Some(writer) = &item.writer {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Writer: ", key), Span::styled(writer.clone(),
                                    white),
                                ],
                            ),
                        );
                }
                if !item.starring.is_empty() {
                    credits
                        .push(
                            Line::from(
                                vec![
                                    Span::styled("Starring: ", key), Span::styled(item.starring
                                    .join(", "), white),
                                ],
                            ),
                        );
                }
                // Round 64: a `----` separator between the description body
                // and the credits block (user's Info-box mockup).
                if !credits.is_empty() {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                Span::styled(
                                    " ----",
                                    dim,
                                ),
                            ),
                        ),
                    );
                }
            }
            Some(item) => {
                rows.push(ListItem::new(Line::styled(" --- [Item]", group)));
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Name", key), Span::raw(": "), Span::raw(item
                                .name.clone()),
                            ],
                        ),
                    ),
                );
                if let Some(artist) = &item.artist {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Artist", key), Span::raw(": "),
                                    Span::raw(artist.clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(artist) = &item.album_artist {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Album artist", key), Span::raw(": "),
                                    Span::raw(artist.clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(album) = &item.album {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Album", key), Span::raw(": "), Span::raw(album
                                    .clone()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(year) = item.year {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Year", key), Span::raw(": "), Span::raw(year
                                    .to_string()),
                                ],
                            ),
                        ),
                    );
                }
                if let Some(secs) = item.runtime_secs {
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Duration", key),
                                    Span::raw(format!(": {}:{:02}", secs / 60, secs % 60)),
                                ],
                            ),
                        ),
                    );
                }
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Type", key), Span::raw(": "), Span::raw(item
                                .kind.clone()),
                            ],
                        ),
                    ),
                );
            }
            None => {
                rows.push(ListItem::new(Line::styled(" --- [Server]", group)));
                if let Some(err) = &self.error {
                    rows.push(ListItem::new(Line::styled(err.clone(), dim)));
                } else {
                    let label = self
                        .selected
                        .as_ref()
                        .map(|k| k.label())
                        .unwrap_or_else(|| "No library selected".to_owned());
                    rows.push(
                        ListItem::new(
                            Line::from(
                                vec![
                                    Span::styled("Library", key), Span::raw(": "),
                                    Span::raw(label),
                                ],
                            ),
                        ),
                    );
                    if self.items.is_empty() {
                        rows.push(
                            ListItem::new(
                                Line::from(
                                    vec![
                                        Span::styled("Hint", key), Span::raw(": "),
                                        Span::styled("Pick a library on the left.", dim),
                                    ],
                                ),
                            ),
                        );
                    }
                }
            }
        }
        if let Some((_, song)) = ctx.find_current_song_in_queue()
            && let Some(server) = &self.server && song.file.starts_with(&server.base)
            && ctx.status.state == State::Play
        {
            rows.push(ListItem::new(""));
            rows.push(ListItem::new(Line::styled(" --- [Now playing]", group)));
            if let Some(title) = song.metadata.get("title") {
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Title", key), Span::raw(": "), Span::raw(title
                                .join(", ").into_owned()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(artist) = song.metadata.get("artist") {
                rows.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Artist", key), Span::raw(": "),
                                Span::raw(artist.join(", ").into_owned()),
                            ],
                        ),
                    ),
                );
            }
        }
        let block = ratatui::widgets::Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        // Round 64: the poster/preview moved out of the Info box into the
        // small shelf beside the list (see `layout_vertical`); the Info
        // text reclaims the whole inner width. When the terminal is too
        // short for the shelf the poster is hidden instead of re-tiling
        // the Info box.
        if !self.is_modal_open {
            match self.poster_shelf {
                // Round 67 (user): no framing border around the preview —
                // the adaptive border resized while the artwork loaded and
                // was distracting. The image keeps its exact size/position
                // (drawn in the same area as before, just without the box).
                Some(shelf) => {
                    let inner = Rect {
                        x: shelf.x.saturating_add(1),
                        y: shelf.y.saturating_add(1),
                        width: shelf.width.saturating_sub(2),
                        height: shelf.height.saturating_sub(2),
                    };
                    if inner.width >= 3 && inner.height >= 3 {
                        self.poster.draw(inner, ctx);
                    } else {
                        self.poster.hide(ctx);
                    }
                }
                None => self.poster.hide(ctx),
            }
        }
        // Round 64-2 (user): the Info text keeps a one-cell margin on both
        // sides (the mockup's `│ Description ↴` / indented description).
        let text_area = Rect {
            x: inner.x.saturating_add(1),
            y: inner.y,
            width: inner.width.saturating_sub(2),
            height: inner.height,
        };
        let header_h = usize::from(header_title.is_some())
            + usize::from(
                !header_episode_left.is_empty() || !header_episode_right.is_empty(),
            ) + usize::from(header_desc);
        let credits_h = credits.len();
        let (header_area, body_area, credits_area) = if header_h > 0
            && header_h + credits_h < text_area.height as usize
        {
            let [a, b, c] = Layout::vertical([
                    Constraint::Length(header_h as u16),
                    Constraint::Min(0),
                    Constraint::Length(credits_h as u16),
                ])
                .areas(text_area);
            (a, b, c)
        } else {
            (Rect::default(), text_area, Rect::default())
        };
        if header_h > 0 && header_area.height > 0 {
            let time_w = (header_time.chars().count() + 4) as u16;
            let (title_area, time_area) = {
                let [a, b] = Layout::horizontal([
                        Constraint::Min(0),
                        Constraint::Length(time_w),
                    ])
                    .areas(header_area);
                (a, b)
            };
            let prefix = header_prefix.unwrap_or_default();
            let prefix_w = prefix.chars().count() as u16;
            let (prefix_area, marquee_area) = if prefix_w > 0
                && prefix_w < title_area.width
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
                        Paragraph::new(Span::styled(prefix, base)),
                        prefix_area,
                    );
            }
            let title = header_title.unwrap_or_default();
            let title_len = title.chars().count() as u16;
            let offset = if title_len > marquee_area.width {
                let elapsed_ms = self
                    .info_song_shown_at
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
                &Line::from(Span::styled(title, base)),
                offset,
                base,
            );
            if !header_time.is_empty() {
                frame
                    .render_widget(
                        Paragraph::new(Span::styled(header_time, bold))
                            .alignment(Alignment::Right),
                        time_area,
                    );
            }
            let has_episode = !header_episode_left.is_empty()
                || !header_episode_right.is_empty();
            if has_episode {
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
                if !header_episode_left.is_empty() {
                    frame
                        .render_widget(
                            Paragraph::new(Line::from(header_episode_left)),
                            left_area,
                        );
                }
                if !header_episode_right.is_empty() {
                    frame
                        .render_widget(
                            Paragraph::new(Line::from(header_episode_right))
                                .alignment(Alignment::Right),
                            right_area,
                        );
                }
            }
            if header_desc {
                let y = header_area.y + 1 + u16::from(has_episode);
                let row = Rect {
                    x: header_area.x,
                    y,
                    width: header_area.width,
                    height: 1,
                };
                frame
                    .render_widget(
                        Paragraph::new(
                            Line::from(
                                vec![
                                    Span::styled("Description", key), Span::styled(" ↴",
                                    white),
                                ],
                            ),
                        ),
                        row,
                    );
            }
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
        self.info_items_len = rows.len();
        let selection_key = self
            .selected_item()
            .map(|i| i.id)
            .or_else(|| self.selected.as_ref().map(|k| k.id().to_owned()));
        if self.info_song_id != selection_key {
            self.info_song_id = selection_key.clone();
            self.info_state = ListState::default();
            self.info_song_shown_at = Some(std::time::Instant::now());
            self.full_item = None;
            if let Some(id) = selection_key
                && self
                    .selected_item()
                    .is_some_and(|i| matches!(i.kind.as_str(), "Movie" | "Episode"))
            {
                let _ = ctx
                    .work_sender
                    .send(crate::shared::events::WorkRequest::FetchJellyfinItem {
                        item_id: id,
                    });
            }
        }
        if rows.is_empty() || body_area.height == 0 {
            frame.render_widget(block, area);
            self.info_area = body_area;
            self.info_scrollbar_area = Rect::default();
            return;
        }
        let overflow = rows.len() > body_area.height as usize;
        let (list_area, scrollbar_area) = if overflow
            && ctx.config.as_styled_scrollbar().is_some()
        {
            crate::ui::scrollbar_strip(body_area)
        } else {
            (body_area, Rect::default())
        };
        ratatui::widgets::StatefulWidget::render(
            List::new(rows).style(base),
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
            crate::ui::render_scrollbar_strip(
                frame,
                scrollbar,
                scrollbar_area,
                &mut ratatui::widgets::ScrollbarState::new(max_offset + 1)
                    .position(position)
                    .viewport_content_length(list_area.height as usize),
            );
        }
        frame.render_widget(block, area);
        self.info_area = list_area;
        self.info_scrollbar_area = scrollbar_area;
    }
}

impl JellyfinPane {
    /// The search info box: the selected result's details (the standard
    /// key-value layout, no poster).
    pub(super) fn render_search_info(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) {
        let mut items: Vec<ListItem> = Vec::new();
        if let Some(item) = self.search_selected() {
            let key_style = ctx.config.theme.preview_label_style;
            let group = ctx.config.theme.preview_metadata_group_style;
            items.push(ListItem::new(Line::styled(" --- [Item]", group)));
            items.push(
                ListItem::new(
                    Line::from(
                        vec![
                            Span::styled("Title", key_style), Span::raw(": "),
                            Span::raw(item.name.clone()),
                        ],
                    ),
                ),
            );
            if let Some(artist) = &item.artist {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Artist", key_style), Span::raw(": "),
                                Span::raw(artist.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(album) = &item.album {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Album", key_style), Span::raw(": "),
                                Span::raw(album.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(series) = &item.series_name {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Series", key_style), Span::raw(": "),
                                Span::raw(series.clone()),
                            ],
                        ),
                    ),
                );
            }
            if let Some(year) = item.year {
                items.push(
                    ListItem::new(
                        Line::from(
                            vec![
                                Span::styled("Year", key_style), Span::raw(": "),
                                Span::raw(year.to_string()),
                            ],
                        ),
                    ),
                );
            }
            items.push(
                ListItem::new(
                    Line::from(
                        vec![
                            Span::styled("Type", key_style), Span::raw(": "),
                            Span::raw(item.kind.clone()),
                        ],
                    ),
                ),
            );
        } else {
            items.push(ListItem::new(
                Line::styled("Type a search term to find items across your libraries", ctx.config.as_list_text_style()),
            ));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_set(ctx.config.as_border_set())
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(area);
        let list = List::new(items).style(ctx.config.as_list_name_style());
        ratatui::widgets::StatefulWidget::render(
            list,
            inner,
            frame.buffer_mut(),
            &mut self.info_state,
        );
        frame.render_widget(block, area);
    }
}

