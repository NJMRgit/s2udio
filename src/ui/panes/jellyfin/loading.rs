use super::*;

impl JellyfinPane {
    /// Load the server credentials (cheap file read; done on first show and
    /// whenever a fetch failed for lack of config).
    pub(super) fn load_server(&mut self, ctx: &Ctx) -> Option<&Jellyfin> {
        if self.server.is_none() {
            let path = ctx.config.jellyfin.config_file.clone();
            let path_str = path.to_string_lossy().into_owned();
            let expanded = crate::config::utils::tilde_expand(&path_str);
            // Round 51: s2udio's own Settings sidecar first
            // (`~/.config/s2udio/jellyfin.ron`, legacy `~/.config/rmpc/…`
            // honored), jellytui's config file is only an optional reuse
            // fallback — same ordering as the playback/MPRIS call sites
            // (`src/core/work.rs` jellyfin_handle).
            let sidecar = crate::config::jellyfin::jellyfin_sidecar_path();
            if let Some(server) =
                Jellyfin::load(std::path::Path::new(expanded.as_ref()), Some(&sidecar))
            {
                self.server = Some(server);
                // A successful load must clear the cached notice; the pane
                // re-attempts the load whenever the error is set.
                self.error = None;
            } else {
                self.error = Some(
                    "Jellyfin is not configured — press Esc → Settings → Jellyfin, \
                     enter Server URL / Username / Password and Sign in."
                        .to_owned(),
                );
            }
        }
        self.server.as_ref()
    }
    pub(super) fn fetch_views(&mut self, ctx: &Ctx) {
        if self.load_server(ctx).is_none() {
            return;
        }
        self.request_fetch(ctx, WorkRequest::FetchJellyfinViews);
    }
    /// Round 92: paint the last known library list from disk before the
    /// server answers, so the Libraries tab is populated on its first frame
    /// instead of staying blank until the fetch returns. The pane then
    /// refreshes in the background and rewrites the cache (`JF_VIEWS`).
    /// A cache written for another server/account is ignored; a missing or
    /// unparsable one simply leaves the tab to fill in from the fetch.
    pub(super) fn load_cached_views(&mut self, ctx: &Ctx) {
        if !self.views.is_empty() {
            return;
        }
        let Some((base, user_id)) = self
            .load_server(ctx)
            .map(|server| (server.base.clone(), server.user_id.clone()))
        else {
            return;
        };
        let Some(cache) = crate::jellyfin::load_views_cache(ctx.config.cache_dir.as_deref())
        else {
            return;
        };
        if cache.views.is_empty() {
            return;
        }
        // Identity check: the account must match. The server address alone
        // is NOT enough to reject the cache — the same server is reachable
        // through more than one address (a LAN IP and a localhost proxy),
        // and the cached list is identical for both; a genuinely different
        // account still invalidates it.
        if cache.user_id != user_id {
            log::debug!(
                cached_user:? = cache.user_id, user:? = user_id; "Ignoring the Jellyfin views cache (different account)"
            );
            return;
        }
        if cache.server != base {
            log::debug!(
                cached_server:? = cache.server, server:? = base; "Showing the Jellyfin views cache from another address of the same account"
            );
        }
        log::debug!(views = cache.views.len(), saved_at = cache.saved_at; "Showing the cached Jellyfin views");
        self.views = cache.views;
        self.rebuild_tree();
        if self.selected.is_none() {
            if !self.tree.is_empty() {
                self.tree_list.select(Some(0));
            }
            self.populate_items();
            self.sync_poster(ctx);
        } else {
            self.populate_items();
        }
    }
    /// Round 92: one library fetch started — show the loading indicator and
    /// ask the shared frame clock for its spinner cadence.
    fn begin_fetch(&mut self, ctx: &Ctx) {
        self.fetches_in_flight = self.fetches_in_flight.saturating_add(1);
        self.fetch_activity_at = std::time::Instant::now();
        if self.loading_since.is_none() {
            self.loading_since = Some(std::time::Instant::now());
        }
        self.request_loading_frames(ctx);
    }
    /// Round 92: one tracked fetch finished (result or error).
    fn end_fetch(&mut self, _ctx: &Ctx) {
        self.fetches_in_flight = self.fetches_in_flight.saturating_sub(1);
        self.fetch_activity_at = std::time::Instant::now();
        if self.fetches_in_flight == 0 {
            // Nothing to cancel: the shared clock is only armed by requests,
            // and the next frame asks for none.
            self.loading_since = None;
        }
    }
    /// Result ids whose fetches the loading indicator tracks (poster,
    /// info-item and playback fetches are not part of the "library is
    /// loading" signal).
    fn is_tracked_fetch(id: &str) -> bool {
        matches!(
            id,
            JF_VIEWS
                | JF_HOME
                | JF_WALL_IMAGE
                | JF_ANCESTORS
                | JF_FOLDER
                | JF_ARTISTS
                | JF_ALBUMS
                | JF_SONGS
                | JF_SEARCH
        )
    }
    /// Ask the shared frame clock for the indicator's cadence. Called from
    /// every frame that is still loading; a frame that is not asks nothing and
    /// the clock disarms itself.
    pub(super) fn request_loading_frames(&self, ctx: &Ctx) {
        ctx.animator.request_every(std::time::Duration::from_millis(LOADING_FRAME_MS));
    }
    /// Request one library fetch (round 92: every request goes through
    /// here, so the loading indicator always matches what is in flight; a
    /// send that fails clears the slot again immediately).
    pub(super) fn request_fetch(&mut self, ctx: &Ctx, request: WorkRequest) {
        self.begin_fetch(ctx);
        if ctx.work_sender.send(request).is_err() {
            log::error!("Failed to request jellyfin data");
            self.end_fetch(ctx);
        }
    }
}

impl JellyfinPane {
    pub(super) fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        let MpdQueryResult::Any(any) = data else { return Ok(()) };
        match any.downcast::<crate::jellyfin::JellyfinResult>() {
            Ok(boxed) => {
                // Round 92: a tracked library fetch finished (result or
                // error alike), so its in-flight slot goes away — the last
                // one clears the loading indicator.
                if Self::is_tracked_fetch(id) {
                    self.end_fetch(ctx);
                }
                match (*boxed, id) {
                    (crate::jellyfin::JellyfinResult::Error(_), JF_IMAGE) => {
                        self.poster.clear(ctx);
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Chapters { item_id, chapters },
                        JF_CHAPTERS,
                    ) => {
                        if ctx.mpv.active
                            && ctx.mpv.item_id.as_deref() == Some(item_id.as_str())
                        {
                            ctx.chapters.borrow_mut().insert(item_id, chapters);
                        } else if let Some((_, song)) = ctx.find_current_song_in_queue()
                            && crate::jellyfin::item_id_from_url(&song.file).as_deref()
                                == Some(&item_id)
                        {
                            ctx.chapters
                                .borrow_mut()
                                .insert(song.file.clone(), chapters);
                            ctx.auto_show_chapters();
                        }
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Image { item_id, bytes },
                        JF_IMAGE,
                    ) => {
                        if self.poster.item_id.as_deref() == Some(item_id.as_str()) {
                            self.poster.set_bytes(item_id, bytes);
                        }
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Item(item), JF_ITEM) => {
                        self.full_item = Some(item);
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Error(err), _) => {
                        if id == JF_SEARCH {
                            self.search_pending = false;
                        }
                        // A failed fetch on the way to a search/Home jump
                        // must drop the jump: the tree cache it is waiting
                        // for will never arrive.
                        if self.jump.is_some()
                            && matches!(
                                id,
                                JF_ANCESTORS | JF_FOLDER | JF_ARTISTS | JF_ALBUMS
                                    | JF_SONGS
                            )
                        {
                            self.jump = None;
                        }
                        self.error = Some(err.clone());
                        status_warn!("Jellyfin: {err}");
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Home(home), JF_HOME) => {
                        self.home = *home;
                        self.error = None;
                        self.rebuild_home_sections();
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::TaggedImage { tag, bytes },
                        JF_WALL_IMAGE,
                    ) => {
                        self.home_image_requests.remove(&tag);
                        if bytes.is_empty() {
                            // The server has no image for this item: do not
                            // re-request it every frame.
                            self.home_image_failed.insert(tag);
                        } else {
                            self.home_images.insert(tag, std::sync::Arc::new(bytes));
                        }
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Ancestors { item_id, items },
                        JF_ANCESTORS,
                    ) => {
                        self.on_jump_ancestors(&item_id, items, ctx)?;
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::SearchHints { items }, JF_SEARCH) => {
                        self.search_pending = false;
                        self.search_results = items;
                        // Typed-ahead queries are re-sent until the
                        // results match the current input.
                        let current = ctx.input.value(self.search_buffer).trim().to_owned();
                        if current != self.last_sent_query && !current.is_empty() {
                            self.run_search(ctx);
                        }
                        if let Some(first) = self.search_results.first().cloned() {
                            self.search_results_state.select(Some(0));
                            let _ = first;
                        } else {
                            self.search_results_state = ListState::default();
                        }
                        ctx.render()?;
                    }
                    (crate::jellyfin::JellyfinResult::Views(views), JF_VIEWS) => {
                        self.views = views;
                        self.error = None;
                        // Round 92: remember the fresh list for the next
                        // start (the first frame paints from it). A failed
                        // refresh never reaches here, so the cache is only
                        // ever replaced by server data.
                        if let Some(server) = self.server.as_ref() {
                            crate::jellyfin::save_views_cache(
                                ctx.config.cache_dir.as_deref(),
                                &server.base,
                                &server.user_id,
                                &self.views,
                            );
                        }
                        self.rebuild_tree();
                        if self.selected.is_none() {
                            if !self.tree.is_empty() {
                                self.tree_list.select(Some(0));
                            }
                            self.populate_items();
                            self.sync_poster(ctx);
                        } else {
                            self.populate_items();
                        }
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Children { parent_id, items },
                        JF_FOLDER,
                    ) => {
                        self.folders.insert(parent_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        self.advance_jump(ctx)?;
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Artists { view_id, items },
                        JF_ARTISTS,
                    ) => {
                        self.artists.insert(view_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        self.advance_jump(ctx)?;
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Albums { artist_id, items },
                        JF_ALBUMS,
                    ) => {
                        self.albums.insert(artist_id, items);
                        self.rebuild_tree();
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        self.advance_jump(ctx)?;
                        ctx.render()?;
                    }
                    (
                        crate::jellyfin::JellyfinResult::Songs { album_id, items },
                        JF_SONGS,
                    ) => {
                        self.songs.insert(album_id, items);
                        self.populate_items();
                        self.sync_tree_to_items_cursor();
                        self.sync_poster(ctx);
                        self.advance_jump(ctx)?;
                        ctx.render()?;
                    }
                    _ => {}
                }
            }
            Err(any) => {
                if id == JF_PLAY {
                    self.handle_play_result(any, ctx)?;
                }
            }
        }
        Ok(())
    }
}

