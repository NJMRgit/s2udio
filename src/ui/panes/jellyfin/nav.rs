use super::*;

/// A pending jump that opens the library tree on an item — the target of a
/// search result (or of a Home container) plus the ancestor chain the tree
/// has to expand.
///
/// The jump is asynchronous: `/Items/{id}/Ancestors` names the chain, then
/// every level's children are fetched through the same lazy tree fetches
/// the browser uses, so the tree ends up in exactly the state a manual
/// navigation produces (same nodes, same caches).
#[derive(Debug, Clone, PartialEq)]
pub(super) struct JfJump {
    /// The item to reveal.
    pub(super) target: JfItem,
    /// Start playback once the item is selected (a movie or a specific
    /// episode); containers only open.
    pub(super) play: bool,
    /// `S02`/`season 2` hint: the season to open inside a series target.
    pub(super) season: Option<i32>,
    /// `S02E03` hint: the episode to highlight inside a season target.
    pub(super) episode: Option<i32>,
    /// The ancestor chain, root (library) first, once fetched.
    pub(super) chain: Option<Vec<JfItem>>,
    /// Item ids whose children a fetch was already requested for (the tree
    /// caches are empty until the answer arrives, so a re-check would
    /// otherwise re-request on every unrelated result).
    pub(super) requested: Vec<String>,
    /// Whether a series target has already been re-targeted at the season
    /// its `S##E##` hint names (the second pass then highlights the
    /// episode inside that season).
    pub(super) season_resolved: bool,
}

impl JellyfinPane {
    /// Begin opening the tree on `target`. Switches to the Libraries page
    /// immediately (the user asked for a crossing, not a dialog), then walks
    /// the ancestors as their fetches land.
    pub(super) fn start_jump(
        &mut self,
        target: JfItem,
        play: bool,
        season: Option<i32>,
        episode: Option<i32>,
        ctx: &Ctx,
    ) -> Result<()> {
        if self.mode == JellyfinTabMode::Home {
            self.wall.hide(ctx);
        }
        self.mode = JellyfinTabMode::Libraries;
        self.release_search_input(ctx);
        self.poster.hide(ctx);
        self.jump = Some(JfJump {
            target: target.clone(),
            play,
            season,
            episode,
            chain: None,
            requested: Vec::new(),
            season_resolved: false,
        });
        self.request_fetch(
            ctx,
            WorkRequest::FetchJellyfinAncestors {
                item_id: target.id.clone(),
            },
        );
        ctx.render()?;
        Ok(())
    }
    /// The ancestor chain arrived: record it and start loading the children
    /// the tree needs to show the target.
    pub(super) fn on_jump_ancestors(
        &mut self,
        item_id: &str,
        items: Vec<JfItem>,
        ctx: &Ctx,
    ) -> Result<()> {
        let Some(jump) = self.jump.as_mut() else { return Ok(()) };
        if jump.target.id != item_id {
            return Ok(());
        }
        // The endpoint answers nearest-first; the tree wants root-first.
        // Jellyfin's own `UserRootFolder` ("Media Folders") sits above the
        // libraries the tree shows, so it has no row and is dropped here
        // (verified against the live server: Season, Series, library,
        // UserRootFolder).
        let mut chain = items;
        chain.retain(|item| item.kind != "UserRootFolder");
        chain.reverse();
        jump.chain = Some(chain);
        self.advance_jump(ctx)?;
        Ok(())
    }
    /// After every children fetch: request the next missing level, and
    /// finish the jump once the whole path is loaded.
    pub(super) fn advance_jump(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(jump) = self.jump.as_ref() else { return Ok(()) };
        let Some(chain) = jump.chain.clone() else { return Ok(()) };
        let needed = jump_needed_items(&chain, &jump.target);
        let mut all_loaded = true;
        let mut to_request: Vec<JfNodeKind> = Vec::new();
        for item in &needed {
            let kind = self.node_kind_for(item);
            if self.children_of(&kind).is_none() {
                // A fetch for this level may already be in flight: that is
                // NOT "loaded", the jump has to wait for the answer.
                all_loaded = false;
                if !jump.requested.contains(&kind.id().to_owned()) {
                    to_request.push(kind);
                }
            }
        }
        if all_loaded {
            return self.finish_jump(ctx);
        }
        if to_request.is_empty() {
            // Everything left is already in flight; the next Children
            // result re-runs this.
            return Ok(());
        }
        for kind in to_request {
            if let Some(jump) = self.jump.as_mut() {
                jump.requested.push(kind.id().to_owned());
            }
            self.ensure_loaded(&kind, ctx);
        }
        // Push a frame so the loading of a deeper level is visible (the
        // renders otherwise all happen in the same tick).
        ctx.render()?;
        Ok(())
    }
    /// The whole path is loaded: expand it, move the selection into the
    /// target, and play it when the jump asked for that.
    fn finish_jump(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(jump) = self.jump.clone() else { return Ok(()) };
        let Some(chain) = jump.chain.clone() else { return Ok(()) };
        // "Show S02E03" searched a series by name: the episode hint means
        // one more level — resolve the season among the series' children,
        // retarget the jump at it and let the next pass highlight episode
        // 3 inside it.
        if !jump.season_resolved
            && jump.target.kind == "Series"
            && let Some(episode) = jump.episode
        {
            // The series' seasons are in the tree cache by now (the jump
            // loaded them); `self.items` still belongs to the previous
            // selection at this point.
            let series_kind = self.node_kind_for(&jump.target);
            let seasons = self.children_of(&series_kind).unwrap_or_default();
            let season = seasons
                .iter()
                .find(|item| {
                    item.kind == "Season"
                        && jump
                            .season
                            .is_none_or(|season| item.season_index() == Some(season))
                })
                .cloned();
            if let Some(season) = season {
                let series = jump.target.clone();
                if let Some(jump) = self.jump.as_mut() {
                    // The series is now an ancestor of the new target: keep
                    // it in the chain so the tree expands down to the
                    // season (and the season row itself becomes visible).
                    if let Some(chain) = jump.chain.as_mut() {
                        chain.push(series);
                    }
                    jump.target = season;
                    jump.season = None;
                    jump.episode = Some(episode);
                    jump.season_resolved = true;
                }
                return self.advance_jump(ctx);
            }
        }
        // A series/season opens INTO the target (its seasons/episodes are
        // the right pane); everything else opens beside it in its parent.
        let open_into = matches!(jump.target.kind.as_str(), "Series" | "Season");
        let selected_item = if open_into {
            jump.target.clone()
        } else {
            chain.last().cloned().unwrap_or_else(|| jump.target.clone())
        };
        let selected_kind = self.node_kind_for(&selected_item);
        // The tree is built from `self.views`: a library the jump reached
        // before the views fetch landed is synthesized as a root row, so
        // the expanded path always hangs off a visible node (the next views
        // fetch replaces the list).
        for ancestor in &chain {
            if matches!(ancestor.kind.as_str(), "CollectionFolder" | "UserView")
                && !self.views.iter().any(|view| view.id == ancestor.id)
            {
                self.views.push(ancestor.clone());
            }
        }
        // The tree shows the whole path: every ancestor expanded, plus the
        // selected node itself (so a series' seasons and a season's
        // episodes are visible under it).
        for ancestor in &chain {
            self.expanded.insert(self.node_kind_for(ancestor).key());
        }
        self.expanded.insert(selected_kind.key());
        // The page is scoped to what the user came from instead of showing
        // every library: a library entry shows that library path only, an
        // entry inside a show shows that show (its seasons).
        let scope = self.jump_scope(&chain, &jump.target);
        self.expanded.insert(scope.key());
        self.tree_scope = Some(scope);
        self.selected = Some(selected_kind.clone());
        self.ensure_loaded(&selected_kind, ctx);
        self.rebuild_tree();
        self.populate_items();
        let row = self.jump_highlight_row(&jump, open_into);
        if let Some(row) = row
            && !self.items.is_empty()
        {
            self.item_list.select(Some(row.min(self.items.len() - 1)));
            *self.item_list.offset_mut() = 0;
        }
        self.sync_tree_to_items_cursor();
        self.sync_poster(ctx);
        self.scroll_tree_selection_into_view(ctx);
        self.scroll_items_selection_into_view(ctx);
        // Drop the jump before playing: the play path can prompt, and a
        // leftover jump would re-run on the next children result.
        self.jump = None;
        // A container target (a series/season reached with an S##E## hint)
        // played its highlighted row; everything else plays the item the jump
        // targeted, not the row the cursor fell back to — an item the parent's
        // list does not contain (a Home card whose season fetch has not landed
        // yet) must still start, and it must never start the wrong thing.
        if jump.play {
            if open_into {
                self.play_selected(ctx)?;
            } else {
                // The tree's copy of the item is what plays (see
                // [`Self::jump_play_item`]): it carries the `UserData` a
                // `SearchHint` lacks, so a partly watched result opens the
                // Continue / Start over question **before** mpv is spawned and
                // nothing plays until the answer (2026-10-29).
                let item = self.jump_play_item(&jump);
                self.play_item(&item, ctx)?;
            }
        }
        ctx.render()?;
        Ok(())
    }
    /// The item a jump starts, **as the freshly populated tree holds it**: the
    /// parent's list carries the `UserData` and `SeasonId` a `SearchHints`
    /// result lacks, so a partly watched item's Continue / Start over question
    /// is asked before mpv starts and an episode plays with its season
    /// playlist. Falls back to the jump target when the parent's list does not
    /// contain it (a Home card whose season fetch has not landed yet).
    fn jump_play_item(&self, jump: &JfJump) -> JfItem {
        self.items
            .iter()
            .find(|item| item.id == jump.target.id)
            .cloned()
            .unwrap_or_else(|| jump.target.clone())
    }
    /// Which row of the freshly populated items pane the cursor goes to
    /// (`None` when the item is not in the list at all).
    fn jump_highlight_row(&self, jump: &JfJump, open_into: bool) -> Option<usize> {
        if self.items.is_empty() {
            return None;
        }
        let find = |pred: &dyn Fn(&JfItem) -> bool| self.items.iter().position(|i| pred(i));
        if open_into {
            Some(match jump.target.kind.as_str() {
                // A series opens on the hinted season (S02/season 2) — or
                // its first season.
                "Series" => jump
                    .season
                    .and_then(|season| {
                        find(&|item: &JfItem| item.season_index() == Some(season))
                    })
                    .unwrap_or(0),
                // A season opens on episode N (S02E03) or episode 1.
                "Season" => jump
                    .episode
                    .and_then(|episode| {
                        find(&|item: &JfItem| item.index_number == Some(episode))
                    })
                    .unwrap_or(0),
                _ => 0,
            })
        } else {
            find(&|item: &JfItem| item.id == jump.target.id)
        }
    }
    /// The tree scope a jump enters with: the show whose seasons the user
    /// came to see when the target (or an ancestor) is a series, otherwise the
    /// library the item lives in.
    pub(super) fn jump_scope(&self, chain: &[JfItem], target: &JfItem) -> JfNodeKind {
        let show = chain
            .iter()
            .chain(std::iter::once(target))
            .find(|item| item.kind == "Series");
        if let Some(show) = show {
            return self.node_kind_for(show);
        }
        match chain.first() {
            Some(view) if matches!(view.kind.as_str(), "CollectionFolder" | "UserView") => {
                self.node_kind_for(view)
            }
            _ => self.node_kind_for(target),
        }
    }
    /// The item as the tree will hold it: a View that the server's `views`
    /// list already knows is matched to that exact object (same key, same
    /// label), everything else keeps its own kind.
    pub(super) fn node_kind_for(&self, item: &JfItem) -> JfNodeKind {
        if let Some(view) = self.views.iter().find(|view| view.id == item.id) {
            return JfNodeKind::View(view.clone());
        }
        match item.kind.as_str() {
            "MusicArtist" => JfNodeKind::Artist(item.clone()),
            "MusicAlbum" => JfNodeKind::Album(item.clone()),
            "CollectionFolder" | "UserView" => JfNodeKind::View(item.clone()),
            _ => JfNodeKind::Folder(item.clone()),
        }
    }
}

/// The nodes whose children the tree needs in order to show the jump's
/// target: every ancestor, plus the target itself when it opens INTO the
/// target (its seasons/episodes are the right pane).
fn jump_needed_items(chain: &[JfItem], target: &JfItem) -> Vec<JfItem> {
    let mut needed: Vec<JfItem> = chain.to_vec();
    if matches!(target.kind.as_str(), "Series" | "Season") {
        needed.push(target.clone());
    }
    needed
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn item(id: &str, kind: &str) -> JfItem {
        JfItem {
            id: id.to_owned(),
            name: id.to_owned(),
            kind: kind.to_owned(),
            ..Default::default()
        }
    }

    #[test]
    fn a_series_jump_loads_the_series_itself_not_only_its_parent() {
        let library = item("lib", "CollectionFolder");
        let series = item("series", "Series");
        let needed = jump_needed_items(std::slice::from_ref(&library), &series);
        let ids: Vec<&str> = needed.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["lib", "series"]);

        // A movie only needs its parent's children (the row it sits on).
        let movie = item("movie", "Movie");
        let needed = jump_needed_items(std::slice::from_ref(&library), &movie);
        let ids: Vec<&str> = needed.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["lib"]);
    }

    #[test]
    fn the_ancestor_chain_is_reversed_to_root_first() {
        // `/Items/{id}/Ancestors` answers nearest-first; a jump reverses it
        // so tree keys expand from the library down.
        let mut chain = vec![item("season", "Season"), item("series", "Series"),
                             item("lib", "CollectionFolder")];
        chain.reverse();
        let ids: Vec<&str> = chain.iter().map(|i| i.id.as_str()).collect();
        assert_eq!(ids, ["lib", "series", "season"]);
    }
}
