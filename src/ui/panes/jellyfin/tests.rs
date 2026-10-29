    use super::*;
    use crate::{
        MpdQueryResult,
        jellyfin::JellyfinResult,
        shared::events::WorkRequest,
    };

    /// Render the pane at `width`x`height` and return the text grid.
    fn render_text_at(
        pane: &mut JellyfinPane,
        ctx: &crate::ctx::Ctx,
        width: u16,
        height: u16,
    ) -> String {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                pane.render(frame, ratatui::prelude::Rect::new(0, 0, width, height), ctx)
                    .unwrap()
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buf[(x, y)].symbol().to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn render_text(pane: &mut JellyfinPane, ctx: &crate::ctx::Ctx) -> String {
        render_text_at(pane, ctx, 100, 40)
    }

    /// The tree-browser args drive the shared split: `tree_min_width: 60`
    /// widens the tree at 160 cols (vs the 50 default) and the default
    /// hide threshold still collapses it on narrow TUIs.
    #[test]
    fn tree_args_change_the_tree_width() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        pane.tree_args = TreeBrowserArgs { tree_min_width: 60, ..TreeBrowserArgs::default() };
        let (tree, _right) = pane.split_tree(ratatui::prelude::Rect::new(0, 0, 160, 30));
        assert_eq!(tree.width, 60, "tree_min_width: 60 widens the tree at 160 cols");
        let (tree, _right) = pane.split_tree(ratatui::prelude::Rect::new(0, 0, 100, 30));
        assert_eq!(tree.width, 0, "the default hide threshold still hides at 100 cols");
    }

    /// Selecting an episode fetches the full item (like the queue tab's info
    /// box) and the info box then shows the overview + credits.
    #[test]
    fn selecting_an_episode_fetches_and_shows_the_full_item_info() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        // The tab now opens on Home; this test is about the browser.
        pane.mode = JellyfinTabMode::Libraries;

        // A browser list item: the list fetch omits overview/credits.
        let list_item = crate::jellyfin::JfItem {
            id: "item-1".to_owned(),
            name: "Mars University".to_owned(),
            kind: "Episode".to_owned(),
            series_name: Some("Futurama".to_owned()),
            season_number: Some(2),
            index_number: Some(2),
            year: Some(1999),
            runtime_secs: Some(1352),
            ..Default::default()
        };
        pane.items = vec![list_item.clone()];
        pane.item_list.select(Some(0));

        // First render: header + episode row from the list item, and the
        // full item fetch is sent.
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("1999 -- Futurama"), "{text}");
        assert!(text.contains("S02E02"), "{text}");
        let sent = work_rx.try_recv().expect("the full item fetch is requested");
        assert!(matches!(
            sent,
            WorkRequest::FetchJellyfinItem { ref item_id } if item_id == "item-1"
        ));

        // The full item arrives (overview + credits, as the server returns
        // with Fields=Chapters,People).
        let full = crate::jellyfin::JfItem {
            id: "item-1".to_owned(),
            name: "Mars University".to_owned(),
            kind: "Episode".to_owned(),
            series_name: Some("Futurama".to_owned()),
            season_number: Some(2),
            index_number: Some(2),
            year: Some(1999),
            runtime_secs: Some(1352),
            overview: Some("Fry, Leela and Bender go to Mars University.".to_owned()),
            director: Some("Bret Haaland".to_owned()),
            writer: Some("J. Stewart Burns".to_owned()),
            starring: vec!["Billy West".to_owned(), "Katey Sagal".to_owned()],
            ..Default::default()
        };
        pane.on_query_finished(
            crate::ui::panes::jellyfin::JF_ITEM,
            MpdQueryResult::Any(Box::new(JellyfinResult::Item(full))),
            true,
            &ctx,
        )
        .unwrap();

        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("Fry, Leela and Bender go to Mars"), "overview: {text}");
        assert!(text.contains("University."), "overview wrap: {text}");
        assert!(text.contains("Director: Bret Haaland"), "{text}");
        assert!(text.contains("Writer: J. Stewart Burns"), "{text}");
        assert!(text.contains("Starring: Billy West, Katey Sagal"), "{text}");
    }



    /// Both panes share the same selection: the right pane always shows the
    /// current node's children (a library shows its shows, a series its
    /// seasons), and at the root it lists every library. `a` / `←` backs
    /// out one level with the row we came from highlighted.
    #[test]
    fn root_lists_all_libraries_and_back_out_navigates_up() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        // The tab now opens on Home; this test is about the browser.
        pane.mode = JellyfinTabMode::Libraries;

        let animation = crate::jellyfin::JfItem {
            id: "anim".to_owned(),
            name: "Animation".to_owned(),
            kind: "CollectionFolder".to_owned(),
            ..Default::default()
        };
        let anime = crate::jellyfin::JfItem {
            id: "anime".to_owned(),
            name: "Anime".to_owned(),
            kind: "CollectionFolder".to_owned(),
            ..Default::default()
        };
        let futurama = crate::jellyfin::JfItem {
            id: "futu".to_owned(),
            name: "Futurama".to_owned(),
            kind: "Series".to_owned(),
            ..Default::default()
        };
        let season1 = crate::jellyfin::JfItem {
            id: "s1".to_owned(),
            name: "Season 1".to_owned(),
            kind: "Season".to_owned(),
            ..Default::default()
        };

        // Root (selected = None): the right pane lists every library.
        pane.views = vec![animation.clone(), anime.clone()];
        pane.selected = None;
        pane.populate_items();
        assert_eq!(pane.items.len(), 2, "root lists all libraries");
        assert_eq!(pane.items[0].id, "anim");
        assert_eq!(pane.items[1].id, "anime");

        // At the root the tree highlight follows the right-pane cursor.
        pane.rebuild_tree();
        pane.move_items(1, &ctx).unwrap();
        assert_eq!(pane.items[0].id, "anim");
        assert_eq!(pane.item_list.selected(), Some(1), "cursor moved to the second library");
        assert_eq!(pane.tree_list.selected(), Some(1), "tree follows the library cursor");
        pane.move_items(-1, &ctx).unwrap();
        assert_eq!(pane.tree_list.selected(), Some(0), "tree follows back");

        // A library always shows its children.
        let episode1 = crate::jellyfin::JfItem {
            id: "ep1".to_owned(),
            name: "Space Pilot 3000".to_owned(),
            kind: "Episode".to_owned(),
            ..Default::default()
        };
        pane.folders.insert("anim".to_owned(), vec![futurama.clone()]);
        pane.folders.insert("futu".to_owned(), vec![season1.clone()]);
        pane.folders.insert("s1".to_owned(), vec![episode1.clone()]);
        pane.selected = Some(JfNodeKind::View(animation.clone()));
        pane.populate_items();
        assert_eq!(pane.items.len(), 1);
        assert_eq!(pane.items[0].id, "futu", "a library shows its shows");

        // Entering the season (Animation > Futurama > Season 1) shows the
        // episodes and expands the path in the tree.
        pane.open_item(season1.clone(), &ctx).unwrap();
        assert_eq!(pane.items[0].id, "ep1", "a season shows its episodes");
        pane.rebuild_tree();
        assert!(pane.expanded.contains("view:anim"), "library expanded");
        assert!(pane.expanded.contains("folder:futu"), "series expanded");
        assert!(pane.expanded.contains("folder:s1"), "season expanded");

        // Back out one level: the series' seasons, cursor on the season we
        // came from; the season's branch collapses.
        pane.select_parent(&ctx).unwrap();
        assert_eq!(pane.selected.as_ref().unwrap().id(), "futu");
        assert_eq!(pane.items[0].id, "s1");
        assert!(!pane.expanded.contains("folder:s1"), "season branch collapsed");

        // Back out again: the library's shows, cursor on the series; the
        // series' branch (seasons) collapses and the tree follows the
        // cursor onto the series row.
        pane.select_parent(&ctx).unwrap();
        assert_eq!(pane.selected.as_ref().unwrap().id(), "anim");
        assert_eq!(pane.items[0].id, "futu");
        assert!(!pane.expanded.contains("folder:futu"), "series branch collapsed");
        assert!(pane.expanded.contains("view:anim"), "library still expanded");
        let futu_row = pane
            .tree
            .iter()
            .position(|node| node.kind.item().id == "futu")
            .expect("futurama visible in the tree");
        assert_eq!(pane.tree_list.selected(), Some(futu_row), "tree follows the series cursor");

        // Moving the cursor through the library's shows follows the tree.
        pane.move_items(0, &ctx).unwrap();
        assert_eq!(pane.tree_list.selected(), Some(futu_row));

        // Back out all the way: the root lists all libraries again, with
        // the library we came from highlighted in the right pane; the
        // library's branch collapses.
        pane.select_parent(&ctx).unwrap();
        assert!(pane.selected.is_none(), "backed out to the root");
        assert_eq!(pane.items.len(), 2, "root lists all libraries again");
        assert_eq!(pane.items[0].id, "anim");
        assert!(!pane.expanded.contains("view:anim"), "library branch collapsed");

        // Backing out at the root is a no-op.
        pane.select_parent(&ctx).unwrap();
        assert!(pane.selected.is_none());
        assert_eq!(pane.items.len(), 2);
    }



    /// On TUIs wider than 120 columns the left library tree keeps a
    /// 50-column minimum, and the items pane still gets the remainder.
    #[test]
    fn tree_pane_keeps_min_width_on_wide_tui() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        pane.mode = JellyfinTabMode::Libraries;
        pane.views = vec![crate::jellyfin::JfItem {
            id: "anim".to_owned(),
            name: "Animation".to_owned(),
            kind: "CollectionFolder".to_owned(),
            ..Default::default()
        }];
        pane.selected = None;
        pane.populate_items();
        pane.item_list.select(Some(0));

        let backend = ratatui::backend::TestBackend::new(160, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                pane.render(frame, ratatui::prelude::Rect::new(0, 0, 160, 30), &ctx).unwrap()
            })
            .unwrap();
        assert_eq!(
            pane.tree_area.width, 48,
            "50-col tree pane minus its 2 border columns: {:?}",
            pane.tree_area
        );
        assert!(pane.items_area.x >= pane.tree_area.width, "items pane starts after the tree");
    }

    /// The items-box title keeps the pre-Phase-2 jellyfin spacing: padded
    /// on both sides with the count after a space — `" Items (3) "` (Phase
    /// 2.1 parity close-out; the shared render flattened it to
    /// `" Items(3) "`).
    #[test]
    fn items_box_title_keeps_the_jellyfin_spacing() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        pane.mode = JellyfinTabMode::Libraries;
        pane.items = vec![
            crate::jellyfin::JfItem {
                id: "i1".to_owned(),
                name: "One".to_owned(),
                kind: "Episode".to_owned(),
                ..Default::default()
            },
            crate::jellyfin::JfItem {
                id: "i2".to_owned(),
                name: "Two".to_owned(),
                kind: "Episode".to_owned(),
                ..Default::default()
            },
            crate::jellyfin::JfItem {
                id: "i3".to_owned(),
                name: "Three".to_owned(),
                kind: "Episode".to_owned(),
                ..Default::default()
            },
        ];
        let text = render_text(&mut pane, &ctx);
        assert!(
            text.contains(" Items (3) "),
            "the items-box title is ` Items (3) `, not ` Items(3) `: {text}"
        );
    }

    /// The temp-play entry is cleared on the stop transition everywhere —
    /// pins the unified Stop cleanup (before Phase 2, jellyfin's two stop
    /// arms disagreed on whether `ctx.temp_play_id` was cleared).
    #[test]
    fn stop_clears_the_temp_play_id() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        pane.temp_play_id = Some(7);
        ctx.temp_play_id.set(Some(7));
        ctx.status.state = crate::mpd::commands::State::Stop;

        let mut event = crate::ui::UiEvent::PlaybackStateChanged;
        pane.on_event(&mut event, true, &ctx).unwrap();
        assert_eq!(pane.temp_play_id, None);
        assert_eq!(ctx.temp_play_id.get(), None, "the queue pane reads it via Ctx");
    }

    /// A pane wired to a fake server + the default test context.
    /// The frame at a terminal width where the library tree is drawn (the
    /// tree pane hides below ~120 columns, so the 100-col `render_text`
    /// never shows it).
    fn render_text_wide(pane: &mut JellyfinPane, ctx: &crate::ctx::Ctx) -> String {
        let backend = ratatui::backend::TestBackend::new(200, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                pane.render(frame, ratatui::prelude::Rect::new(0, 0, 200, 40), ctx).unwrap()
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..40u16)
            .map(|y| (0..200).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn pane_with_server() -> (JellyfinPane, crate::ctx::Ctx, crossbeam::channel::Receiver<WorkRequest>) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = JellyfinPane::new(&ctx);
        pane.server = Some(crate::jellyfin::Jellyfin {
            base: "http://jf:8096".to_owned(),
            token: "tok".to_owned(),
            user_id: "u".to_owned(),
        });
        (pane, ctx, work_rx)
    }

    fn jf_item(id: &str, name: &str, kind: &str) -> crate::jellyfin::JfItem {
        crate::jellyfin::JfItem {
            id: id.to_owned(),
            name: name.to_owned(),
            kind: kind.to_owned(),
            ..Default::default()
        }
    }

    fn feed(pane: &mut JellyfinPane, ctx: &crate::ctx::Ctx, id: &'static str,
            result: JellyfinResult) {
        pane.on_query_finished(
            id,
            MpdQueryResult::Any(Box::new(result)),
            true,
            ctx,
        )
        .unwrap();
    }

    /// The tab opens on Home (the Jellyfin web UI's landing page). The
    /// library tree is now a sub-page of Home, so the toggle row is just
    /// Home · Search.
    #[test]
    fn the_tab_opens_on_home_and_shows_home_and_search() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        assert_eq!(pane.mode, JellyfinTabMode::Home);
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("● Home"), "the home segment is active: {text}");
        assert!(text.contains("⭘ Search"), "{text}");
        assert!(!text.contains("Libraries  ⭘"), "no Libraries segment: {text}");
    }

    /// Shift+Tab cycles Home <-> Search; the tree page counts as Home.
    #[test]
    fn shift_tab_cycles_home_and_search() {
        let (mut pane, mut ctx, _work_rx) = pane_with_server();
        pane.toggle_mode(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Search);
        pane.toggle_mode(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Home);
        pane.mode = JellyfinTabMode::Libraries;
        pane.toggle_mode(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Search);
    }

    /// The tree page carries a `⌂ Home` button back to the carousels; the
    /// carousels have none. Leaving the tree for Home collapses it.
    #[test]
    fn the_tree_page_has_a_home_button() {
        let (mut pane, ctx) = home_rig();
        pane.views = vec![jf_item("lib", "Anime", "CollectionFolder")];
        pane.mode = JellyfinTabMode::Libraries;
        let kind = pane.node_kind_for(&jf_item("lib", "Anime", "CollectionFolder"));
        pane.selected = Some(kind.clone());
        pane.expanded.insert(kind.key());
        pane.rebuild_tree();
        let _ = render_text(&mut pane, &ctx);
        let button = pane.home_button_area;
        assert!(button.width > 0, "a Home button on the tree page");
        let event = MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: button.x + 1,
            y: button.y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        };
        assert!(pane.handle_mouse_event(event, &ctx).is_ok());
        assert_eq!(pane.mode, JellyfinTabMode::Home);
        assert!(
            pane.expanded.is_empty() && pane.selected.is_none(),
            "going back to Home collapses the tree"
        );
        // And it is not drawn on the Home page itself.
        let _ = render_text(&mut pane, &ctx);
        assert_eq!(pane.home_button_area.width, 0);
    }

    /// Left on the tree's root (the library view itself) leaves for Home —
    /// the `⌂ Home` button's keyboard twin — and folds the tree up, so the
    /// next visit starts from the library tiles.
    #[test]
    fn left_at_the_tree_root_goes_home() {
        let (mut pane, ctx) = home_rig();
        pane.views = vec![jf_item("lib", "Anime", "CollectionFolder")];
        pane.mode = JellyfinTabMode::Libraries;
        let kind = pane.node_kind_for(&jf_item("lib", "Anime", "CollectionFolder"));
        pane.selected = Some(kind.clone());
        pane.expanded.insert(kind.key());
        pane.rebuild_tree();
        pane.select_parent(&ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Home, "Left at the root leaves the tree");
        assert!(pane.expanded.is_empty(), "the tree is collapsed on the way out");
        assert!(pane.selected.is_none(), "nothing stays selected behind the carousels");
    }

    /// Esc on the search page returns to the page the search was entered
    /// from (Home, when the `S` jump was used there), and the search state
    /// is cleared.
    #[test]
    fn search_returns_to_the_page_it_came_from() {
        let (mut pane, mut ctx, _work_rx) = pane_with_server();
        pane.jump_to_search(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Search);
        assert_eq!(pane.search_return, JellyfinTabMode::Home);
        pane.search_results = vec![jf_item("x", "Result", "Movie")];
        pane.search_results_state.select(Some(0));
        pane.exit_search(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Home);
        assert!(pane.search_results.is_empty(), "the search page clears on exit");

        // Jumping from the browser returns to the browser.
        pane.mode = JellyfinTabMode::Libraries;
        pane.jump_to_search(&mut ctx).unwrap();
        pane.exit_search(&mut ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Libraries);
    }

    fn home_rig() -> (JellyfinPane, crate::ctx::Ctx) {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        pane.mode = JellyfinTabMode::Home;
        // Deterministic card sizes: a typical 8x18 px terminal cell.
        pane.home_cell_ratio = Some(2.25);
        (pane, ctx)
    }

    fn shelf_home() -> crate::jellyfin::JfHome {
        let mut resume = jf_item("ep1", "Legendary Class", "Episode");
        resume.series_name = Some("Overgeared".to_owned());
        resume.season_number = Some(1);
        resume.index_number = Some(1);
        resume.runtime_secs = Some(1421);
        resume.playback_position_secs = 644.0;
        let mut resume2 = jf_item("ep2", "Second Class", "Episode");
        resume2.series_name = Some("Overgeared".to_owned());
        resume2.season_number = Some(1);
        resume2.index_number = Some(2);
        let mut next = jf_item("ep3", "We Is Us", "Episode");
        next.series_id = Some("pluribus".to_owned());
        let latest = jf_item("s1", "Lanterns", "Series");
        let mut recent_episode = jf_item("ep9", "Pilot", "Episode");
        recent_episode.series_id = Some("lanterns-series".to_owned());
        crate::jellyfin::JfHome {
            continue_watching: vec![resume, resume2],
            next_up: vec![next],
            recently_added: vec![latest, recent_episode],
            library_latest: Vec::new(),
        }
    }

    fn library_strip(name: &str, ids: &[&str]) -> crate::jellyfin::JfLibraryLatest {
        crate::jellyfin::JfLibraryLatest {
            library: jf_item(&format!("lib-{name}"), name, "CollectionFolder"),
            items: ids.iter().map(|id| jf_item(id, id, "Movie")).collect(),
        }
    }

    /// One carousel per non-empty shelf, each with its own cursor.
    #[test]
    fn the_home_page_is_one_carousel_per_shelf() {
        let (mut pane, _ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let shelves: Vec<home::HomeShelf> =
            pane.home_sections.iter().map(|s| s.shelf).collect();
        assert_eq!(
            shelves,
            [
                home::HomeShelf::ContinueWatching,
                home::HomeShelf::NextUp,
                home::HomeShelf::RecentlyAdded
            ]
        );
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep1".to_owned()));

        // An empty shelf is not a carousel at all.
        pane.home.next_up.clear();
        pane.rebuild_home_sections();
        assert_eq!(pane.home_sections.len(), 2);
        assert_eq!(
            pane.home_sections.iter().map(|s| s.shelf).collect::<Vec<_>>(),
            [home::HomeShelf::ContinueWatching, home::HomeShelf::RecentlyAdded]
        );
    }

    /// Up/Down move between sections; Left/Right (a/d) move the focused
    /// carousel and clamp at its ends.
    #[test]
    fn up_down_moves_sections_and_a_d_moves_cards() {
        let (mut pane, mut ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let action = |action: crate::config::keys::DirectoriesActions| {
            crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
                crate::shared::keys::Actions::Directories(action),
            ]))
        };
        // Down: Continue Watching -> Next Up.
        let mut event = action(crate::config::keys::DirectoriesActions::FolderDown);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Section(1));
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep3".to_owned()));
        // Down again: Recently Added; one more: clamped.
        let mut event = action(crate::config::keys::DirectoriesActions::FolderDown);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        let mut event = action(crate::config::keys::DirectoriesActions::FolderDown);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Section(2));

        // Back to Continue Watching, then move right twice (a carousel with
        // two cards clamps on the second).
        let mut event = action(crate::config::keys::DirectoriesActions::FolderUp);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        let mut event = action(crate::config::keys::DirectoriesActions::FolderUp);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        let mut event = action(crate::config::keys::DirectoriesActions::FolderExpand);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep2".to_owned()));
        let mut event = action(crate::config::keys::DirectoriesActions::FolderExpand);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep2".to_owned()));
        let mut event = action(crate::config::keys::DirectoriesActions::FolderCollapse);
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep1".to_owned()));
    }

    /// The carousel scrolls only when the cursor leaves the visible cards.
    #[test]
    fn the_carousel_scrolls_at_the_edges() {
        // Eight cards, five visible: the offset stays put inside the window
        // and follows the cursor once it passes either edge.
        assert_eq!(home::visible_offset(0, 0, 8, 5), 0);
        assert_eq!(home::visible_offset(0, 4, 8, 5), 0);
        assert_eq!(home::visible_offset(0, 5, 8, 5), 1);
        assert_eq!(home::visible_offset(1, 6, 8, 5), 2);
        assert_eq!(home::visible_offset(3, 7, 8, 5), 3, "clamped at the end");
        assert_eq!(home::visible_offset(2, 1, 8, 5), 1, "moves back to the cursor");
        assert_eq!(home::visible_offset(0, 2, 3, 5), 0, "all cards fit");
    }

    /// Previews (Continue Watching) are wide 16:9 cards; posters are 2:3.
    /// The available height is split between the sections.
    #[test]
    fn card_metrics_shape_previews_and_posters() {
        let (img_h, section_h) = home::section_geometry(39, 3);
        assert_eq!(section_h, img_h + 6);
        let preview = home::card_metrics(home::HomeShelf::ContinueWatching, 120, img_h, 2.25);
        let poster = home::card_metrics(home::HomeShelf::NextUp, 120, img_h, 2.25);
        assert_eq!(preview.img_h, poster.img_h);
        assert!(preview.img_w > poster.img_w * 2, "16:9 is twice as wide as 2:3");
        assert!(preview.visible >= 1 && poster.visible >= 1);
        assert!(u16::from(poster.card_h) <= 13, "a third of 39 rows");
        // A tiny pane still produces a usable card.
        let (tiny_img_h, _) = home::section_geometry(9, 3);
        let tiny = home::card_metrics(home::HomeShelf::NextUp, 12, tiny_img_h, 2.25);
        assert!(tiny.img_w >= 2);
        assert!(tiny.visible >= 1);
        // The image height stops at its preferred maximum, so a very tall
        // terminal does not blow one card up over the whole page: extra rows
        // scroll, and the Libraries row only stays on the page when the
        // whole page fits.
        let (img_h, section_h) = home::section_geometry(100, 3);
        assert_eq!(img_h, home::MAX_IMAGE_H);
        assert_eq!(section_h, home::MAX_IMAGE_H + 6);
        let full = 4 * (home::MAX_IMAGE_H + 6);
        assert!(
            !home::libraries_row_fits(full - 1, 3, 3),
            "one row short: the hint stands in for the Libraries row"
        );
        assert!(
            home::libraries_row_fits(full, 3, 3),
            "four rows fit at the preferred card size"
        );
        assert!(
            !home::libraries_row_fits(full, 0, 3),
            "no shelves, nothing to reveal above"
        );
        assert!(
            !home::libraries_row_fits(100, 3, 12),
            "more rows than fit hide the carousel behind Up"
        );
    }

    /// The page paints the three section headers and the card labels, and
    /// shows no Info box (Home has none).
    #[test]
    fn the_home_page_paints_three_carousels_without_an_info_box() {
        let (mut pane, ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("Continue Watching"), "{text}");
        assert!(text.contains("Next Up"), "{text}");
        assert!(text.contains("Recently Added"), "{text}");
        assert!(text.contains("Legendary"), "{text}");
        assert!(text.contains("Lanterns"), "{text}");
        assert!(!text.contains("Info"), "Home has no info box: {text}");
        // The cards were laid out for mouse hit-testing.
        assert!(!pane.home_layout.cards.is_empty());
        assert!(pane.home_layout.cards.iter().all(|card| card.image.width >= 2));
    }

    /// A card click selects it and focuses its carousel.
    #[test]
    fn clicking_a_card_selects_it() {
        let (mut pane, ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let text = render_text(&mut pane, &ctx);
        let _ = text;
        let card = pane
            .home_layout
            .cards
            .iter()
            .find(|card| {
                card.index == 1 && card.row == home::HomeRowId::Section(0)
            })
            .cloned()
            .expect("second Continue Watching card");
        let target = ratatui::layout::Position {
            x: card.rect.x + 1,
            y: card.rect.y + 1,
        };
        let event = MouseEvent {
            kind: MouseEventKind::LeftClick,
            x: target.x,
            y: target.y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        };
        assert!(pane.handle_home_mouse(event, &ctx).unwrap());
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep2".to_owned()));
    }

    /// A Home container card opens the browser instead of playing, and a
    /// Home episode card never starts playback from a jump.
    #[test]
    fn home_containers_open_the_library_page() {
        let (mut pane, ctx, work_rx) = pane_with_server();
        pane.mode = JellyfinTabMode::Home;
        pane.home = crate::jellyfin::JfHome {
            continue_watching: Vec::new(),
            next_up: Vec::new(),
            recently_added: vec![jf_item("series1", "Lanterns", "Series")],
            library_latest: Vec::new(),
        };
        pane.rebuild_home_sections();
        pane.home_activate(&ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Libraries);
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::FetchJellyfinAncestors { ref item_id }) if item_id == "series1"
        ));
    }

    /// A pane whose terminal image backend is kitty, so the poster wall is
    /// active.
    fn kitty_rig()
    -> (JellyfinPane, crate::ctx::Ctx, crossbeam::channel::Receiver<WorkRequest>) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        {
            let mut config = ctx.config.as_ref().clone();
            config.album_art.method = crate::config::album_art::ImageMethod::Kitty;
            ctx.config = std::sync::Arc::new(config);
        }
        let mut pane = JellyfinPane::new(&ctx);
        pane.server = Some(crate::jellyfin::Jellyfin {
            base: "http://jf".to_owned(),
            token: "t".to_owned(),
            user_id: "u".to_owned(),
        });
        pane.mode = JellyfinTabMode::Home;
        pane.home_cell_ratio = Some(2.25);
        (pane, ctx, work_rx)
    }

    /// The Continue Watching card asks for the episode still, and Next Up for
    /// the *series* poster (the web UI's card images).
    #[test]
    fn carousel_images_use_the_right_server_image() {
        let (mut pane, ctx, work_rx) = kitty_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text(&mut pane, &ctx);
        let mut requests = Vec::new();
        while let Ok(request) = work_rx.try_recv() {
            if let WorkRequest::FetchJellyfinImageTagged {
                tag,
                item_id,
                image_type,
                ..
            } = request
            {
                requests.push((tag, item_id, image_type));
            }
        }
        assert!(requests.iter().any(|(tag, id, kind)| {
            id == "ep1" && *kind == crate::jellyfin::JfImageType::Thumb
                && tag.starts_with("Thumb:")
        }), "Continue Watching asks for the episode still: {requests:?}");
        assert!(requests.iter().any(|(_, id, kind)| {
            id == "pluribus" && *kind == crate::jellyfin::JfImageType::Primary
        }), "Next Up asks for the series poster: {requests:?}");
        assert!(requests.iter().any(|(_, id, kind)| {
            id == "lanterns-series" && *kind == crate::jellyfin::JfImageType::Primary
        }), "a recently added episode shows its series poster: {requests:?}");
    }

    /// Every section's cards get their own wall slot: the ids are numbered
    /// across the whole page, not restarted per shelf (which made every
    /// section overwrite the previous one's images, so only the last row
    /// drew).
    #[test]
    fn every_carousel_gets_its_own_image_slots() {
        let (mut pane, ctx, _work_rx) = kitty_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text(&mut pane, &ctx);
        let placements = pane.wall.layout().to_vec();
        assert_eq!(
            placements.len(),
            pane.home_layout.cards.len(),
            "one image slot per visible card"
        );
        let slots: Vec<u16> = placements.iter().map(|p| p.slot).collect();
        let mut unique = slots.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), slots.len(), "slot ids are unique: {slots:?}");
        // The areas span all three carousels (a per-section restart left only
        // the last one).
        let mut rows: Vec<u16> = placements.iter().map(|p| p.area.y).collect();
        rows.sort_unstable();
        rows.dedup();
        assert_eq!(rows.len(), 3, "one image row per carousel: {rows:?}");
    }

    /// Moving the focus down keeps the shelf order fixed: the focused
    /// carousel must not jump to the top of the page (only the styling and,
    /// on short terminals, the vertical scroll change).
    #[test]
    fn moving_the_focus_does_not_reorder_the_sections() {
        let (mut pane, ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text(&mut pane, &ctx);
        let top_before = pane
            .home_layout
            .cards
            .iter()
            .min_by_key(|card| card.rect.y)
            .map(|card| card.row)
            .unwrap();
        assert_eq!(top_before, home::HomeRowId::Section(0),
                   "Continue Watching starts at the top");
        pane.home_focus = home::HomeFocus::Section(1);
        let _ = render_text(&mut pane, &ctx);
        let top_after = pane
            .home_layout
            .cards
            .iter()
            .min_by_key(|card| card.rect.y)
            .map(|card| card.row)
            .unwrap();
        assert_eq!(top_after, home::HomeRowId::Section(0),
                   "the order stays fixed when the focus moves");
        // The focused row is still the second one.
        assert_eq!(pane.home_focus, home::HomeFocus::Section(1));
    }

    /// `S02`, `S02E03`, `season 2` and `2x03` are parsed out of the query;
    /// the free text is what goes to the server.
    #[test]
    fn search_query_hints_are_parsed() {
        let cases: [(&str, &str, Option<i32>, Option<i32>); 5] = [
            ("overgeared S01E03", "overgeared", Some(1), Some(3)),
            ("overgeared s2", "overgeared", Some(2), None),
            ("Overgeared season 2", "Overgeared", Some(2), None),
            ("overgeared 2x03", "overgeared", Some(2), Some(3)),
            ("overgeared", "overgeared", None, None),
        ];
        for (query, text, season, episode) in cases {
            let (got_text, hint) = search::split_query_hint(query);
            assert_eq!(got_text, text, "text for {query:?}");
            assert_eq!(hint.season, season, "season for {query:?}");
            assert_eq!(hint.episode, episode, "episode for {query:?}");
        }
        // A hint-only query still searches its own text.
        let (text, hint) = search::split_query_hint("S02E03");
        assert_eq!(text, "S02E03");
        assert_eq!(hint.season, Some(2));
        assert_eq!(hint.episode, Some(3));
        // A title word that merely starts with `s` is not a hint.
        let (text, hint) = search::split_query_hint("se7en");
        assert_eq!(text, "se7en");
        assert_eq!(hint.season, None);
    }

    /// A pane with a server plus both receivers, so a play path's work
    /// requests and the modals it opens can both be inspected. The video
    /// playback mode is `mpv`, so the only modal a play ever opens is a
    /// question about the play itself.
    fn play_rig() -> (
        JellyfinPane,
        crate::ctx::Ctx,
        crossbeam::channel::Receiver<WorkRequest>,
        crossbeam::channel::Receiver<crate::AppEvent>,
    ) {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (work_tx, work_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        {
            let mut config = ctx.config.as_ref().clone();
            config.video.playback = crate::config::video::VideoPlaybackMode::Mpv;
            ctx.config = std::sync::Arc::new(config);
        }
        let mut pane = JellyfinPane::new(&ctx);
        pane.server = Some(crate::jellyfin::Jellyfin {
            base: "http://jf:8096".to_owned(),
            token: "tok".to_owned(),
            user_id: "u".to_owned(),
        });
        (pane, ctx, work_rx, app_rx)
    }

    /// Every modal the pane opened, drained from the app channel.
    fn opened_modals(
        app_rx: &crossbeam::channel::Receiver<crate::AppEvent>,
    ) -> Vec<crate::AppEvent> {
        let mut modals = Vec::new();
        while let Ok(event) = app_rx.try_recv() {
            if matches!(
                event,
                crate::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_))
            ) {
                modals.push(event);
            }
        }
        modals
    }

    /// Picking a result on the search page leaves the page and asks for the
    /// item's ancestors: the browser then walks the tree to it. An episode
    /// (and a movie) result plays once the tree is on it.
    #[test]
    fn selecting_a_search_result_opens_the_library_page() {
        let (mut pane, ctx, work_rx) = pane_with_server();
        pane.mode = JellyfinTabMode::Search;
        pane.search_results = vec![jf_item("ep2", "Mars University", "Episode")];
        pane.search_results_state.select(Some(0));
        pane.search_activate(&ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Libraries);
        assert!(
            pane.jump.as_ref().is_some_and(|jump| jump.target.id == "ep2" && jump.play),
            "an episode result plays once the tree is open"
        );
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::FetchJellyfinAncestors { ref item_id }) if item_id == "ep2"
        ));
    }

    /// A search result is a place in the library: activating **any** of them
    /// leaves the tree with a selected item. A series only opens (its seasons
    /// are the right pane); nothing plays.
    #[test]
    fn a_series_search_result_opens_into_the_series_and_selects_it() {
        let (mut pane, ctx, work_rx) = pane_with_server();
        pane.views = vec![jf_item("lib", "TV", "CollectionFolder")];
        let series = jf_item("series", "Futurama", "Series");
        let s1 = jf_item("s1", "Season 1", "Season");
        let library = pane.views[0].clone();
        pane.mode = JellyfinTabMode::Search;
        pane.search_results = vec![series.clone()];
        pane.search_results_state.select(Some(0));
        pane.search_activate(&ctx).unwrap();
        assert!(
            pane.jump.as_ref().is_some_and(|jump| !jump.play),
            "a series result only opens, it does not play"
        );
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "series".to_owned(),
            items: vec![library],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![s1],
        });
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("series"));
        assert_eq!(pane.item_list.selected(), Some(0), "season 1 is selected");
        assert!(
            pane.tree_list.selected().is_some(),
            "the series row is selected in the tree"
        );
        let mut requests = Vec::new();
        while let Ok(request) = work_rx.try_recv() {
            requests.push(request);
        }
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, WorkRequest::FetchJellyfinSeason { .. })),
            "nothing is played"
        );
    }

    /// A series result has no play action, so the context-menu key opens
    /// nothing for it — the search menu belongs to the playable video results
    /// (movies/episodes).
    #[test]
    fn a_series_search_result_opens_no_context_menu() {
        let (mut pane, ctx, _work_rx, app_rx) = play_rig();
        pane.mode = JellyfinTabMode::Search;
        pane.search_results = vec![jf_item("series", "Futurama", "Series")];
        pane.search_results_state.select(Some(0));
        pane.search_context_menu(&ctx).unwrap();
        assert!(
            opened_modals(&app_rx).is_empty(),
            "no item menu is opened for a container"
        );
    }

    /// A result with no saved position does not ask: the clicked episode
    /// starts right away (with its season playlist), never the row the cursor
    /// happened to sit on.
    #[test]
    fn a_fresh_search_result_starts_immediately() {
        let (mut pane, ctx, work_rx, app_rx) = play_rig();
        pane.views = vec![jf_item("lib", "TV", "CollectionFolder")];
        let series = jf_item("series", "Futurama", "Series");
        let season = jf_item("season1", "Season 1", "Season");
        let mut ep1 = jf_item("ep1", "Space Pilot 3000", "Episode");
        ep1.season_id = Some("season1".to_owned());
        let mut ep2 = jf_item("ep2", "The Series Has Landed", "Episode");
        ep2.season_id = Some("season1".to_owned());
        let library = pane.views[0].clone();

        pane.mode = JellyfinTabMode::Search;
        pane.search_results = vec![jf_item("ep2", "The Series Has Landed", "Episode")];
        pane.search_results_state.select(Some(0));
        pane.search_activate(&ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "ep2".to_owned(),
            items: vec![season.clone(), series.clone(), library],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![season],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "season1".to_owned(),
            items: vec![ep1, ep2],
        });

        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("season1"));
        assert_eq!(pane.item_list.selected(), Some(1), "the clicked episode");
        assert!(pane.tree_list.selected().is_some());
        let mut requests = Vec::new();
        while let Ok(request) = work_rx.try_recv() {
            requests.push(request);
        }
        assert!(
            requests.iter().any(|request| {
                matches!(
                    request,
                    WorkRequest::FetchJellyfinSeason { season_id, episode_id }
                        if season_id == "season1" && episode_id == "ep2"
                )
            }),
            "the clicked episode starts"
        );
        assert!(opened_modals(&app_rx).is_empty(), "a fresh item asks nothing");
    }

    /// An episode jump expands library → series → season, shows the season's
    /// episodes and puts the selection on the clicked episode.
    #[test]
    fn a_jump_opens_the_tree_on_the_episode() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        pane.views = vec![jf_item("lib", "TV", "CollectionFolder")];
        let series = jf_item("series", "Futurama", "Series");
        let season = jf_item("season1", "Season 1", "Season");
        let ep1 = jf_item("ep1", "Space Pilot 3000", "Episode");
        let ep2 = jf_item("ep2", "The Series Has Landed", "Episode");
        let target = ep2.clone();

        let library = pane.views[0].clone();
        pane.start_jump(target, false, None, None, &ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Libraries);
        // The ancestors answer nearest-first (season, series, library).
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "ep2".to_owned(),
            items: vec![season.clone(), series.clone(), library],
        });
        // Each Children answer triggers the next level's fetch.
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![season.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "season1".to_owned(),
            items: vec![ep1, ep2],
        });

        assert!(pane.jump.is_none(), "the jump finished");
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("season1"));
        assert_eq!(pane.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
                   ["ep1", "ep2"]);
        assert_eq!(pane.item_list.selected(), Some(1), "the clicked episode");
        assert!(pane.expanded.contains("view:lib"));
        assert!(pane.expanded.contains("folder:series"));
        assert!(pane.expanded.contains("folder:season1"));
        assert!(pane.tree.iter().any(|n| n.kind.id() == "season1"),
                "the season is a tree row: {:?}", pane.tree);
        let season_row = pane.tree.iter().position(|n| n.kind.id() == "season1").unwrap();
        assert_eq!(pane.tree_list.selected(), Some(season_row),
                   "the tree highlight follows the items cursor");
    }

    /// The tree page is scoped to what the user came from: an episode entry
    /// shows the show's seasons, not the whole server tree.
    #[test]
    fn an_episode_jump_scopes_the_tree_to_its_show() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        let lib = jf_item("lib", "TV", "CollectionFolder");
        let other = jf_item("lib2", "Movies", "CollectionFolder");
        pane.views = vec![lib.clone(), other];
        // The other library has loaded items: they must not get tree rows.
        pane.folders.insert("lib2".to_owned(), vec![jf_item("movie", "A Movie", "Movie")]);
        let series = jf_item("series", "Futurama", "Series");
        let season = jf_item("s1", "Season 1", "Season");
        let ep = jf_item("e1", "Space Pilot 3000", "Episode");
        pane.start_jump(ep.clone(), false, None, None, &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "e1".to_owned(),
            items: vec![season.clone(), series.clone(), lib],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![season.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "s1".to_owned(),
            items: vec![ep],
        });

        assert_eq!(
            pane.tree_scope.as_ref().map(JfNodeKind::key).as_deref(),
            Some("folder:series"),
            "the entry context is the show"
        );
        assert_eq!(
            pane.tree.first().map(|node| (node.kind.key(), node.depth)),
            Some(("folder:series".to_owned(), 0)),
            "the show is the tree's root row: {:?}",
            pane.tree
        );
        assert!(
            !pane.tree.iter().any(|node| matches!(node.kind.id(), "lib" | "lib2")),
            "no library rows in a show-scoped tree: {:?}",
            pane.tree
        );
        assert!(
            !pane.tree.iter().any(|node| node.kind.id() == "movie"),
            "another library's items are hidden too"
        );
        let season_row = pane
            .tree
            .iter()
            .position(|node| node.kind.id() == "s1")
            .expect("the show's seasons are listed");
        assert_eq!(pane.tree[season_row].depth, 1);

        // And the frame shows the show + its seasons, never the libraries.
        let text = render_text_wide(&mut pane, &ctx);
        assert!(text.contains("Futurama"), "the show is rendered:\n{text}");
        assert!(text.contains("Season 1"), "its seasons are rendered:\n{text}");
        assert!(!text.contains("Movies"), "other libraries are gone:\n{text}");
    }

    /// Selecting a library scopes the tree to that library, expanded: the
    /// other libraries and their items have no rows.
    #[test]
    fn a_library_jump_scopes_the_tree_to_that_library() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        let lib = jf_item("lib", "TV", "CollectionFolder");
        let other = jf_item("lib2", "Movies", "CollectionFolder");
        pane.views = vec![lib.clone(), other];
        let series = jf_item("series", "Futurama", "Series");
        pane.folders.insert("lib".to_owned(), vec![series.clone()]);
        pane.folders.insert("lib2".to_owned(), vec![jf_item("movie", "A Movie", "Movie")]);

        pane.start_jump(lib.clone(), false, None, None, &ctx).unwrap();
        // The library's own ancestors are just Jellyfin's UserRootFolder,
        // which the tree does not show.
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "lib".to_owned(),
            items: vec![jf_item("root", "Media Folders", "UserRootFolder")],
        });

        assert_eq!(
            pane.tree_scope.as_ref().map(JfNodeKind::key).as_deref(),
            Some("view:lib")
        );
        assert_eq!(pane.tree.first().map(|node| node.kind.id()), Some("lib"));
        assert!(
            pane.tree.iter().any(|node| node.kind.id() == "series"),
            "the library is expanded with its items: {:?}",
            pane.tree
        );
        assert!(
            !pane.tree.iter().any(|node| matches!(node.kind.id(), "lib2" | "movie")),
            "the other library and its items are hidden: {:?}",
            pane.tree
        );
    }

    /// Backing out of a scoped root widens the filter one level instead of
    /// selecting a node the tree does not contain.
    #[test]
    fn backing_out_of_the_scoped_root_widens_the_scope() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        let lib = jf_item("lib", "TV", "CollectionFolder");
        pane.views = vec![lib.clone()];
        let series = jf_item("series", "Futurama", "Series");
        let season = jf_item("s1", "Season 1", "Season");
        let ep = jf_item("e1", "Space Pilot 3000", "Episode");
        pane.start_jump(ep, false, None, None, &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "e1".to_owned(),
            items: vec![season.clone(), series.clone(), lib.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![season],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "s1".to_owned(),
            items: vec![jf_item("e1", "Space Pilot 3000", "Episode")],
        });
        assert_eq!(pane.tree_scope.as_ref().map(JfNodeKind::key).as_deref(),
                   Some("folder:series"));

        // s1 -> series -> (out of the scope) library.
        pane.select_parent(&ctx).unwrap();
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("series"));
        assert_eq!(
            pane.tree_scope.as_ref().map(JfNodeKind::key).as_deref(),
            Some("folder:series"),
            "still inside the show"
        );
        pane.select_parent(&ctx).unwrap();
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("lib"));
        assert_eq!(
            pane.tree_scope.as_ref().map(JfNodeKind::key).as_deref(),
            Some("view:lib"),
            "the filter widened to the library"
        );
        assert_eq!(pane.tree.first().map(|node| node.kind.id()), Some("lib"));
    }

    /// Leaving the page for Home drops the filter: the next visit starts
    /// from the full tree again.
    #[test]
    fn returning_home_drops_the_tree_scope() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        let lib = jf_item("lib", "TV", "CollectionFolder");
        let other = jf_item("lib2", "Movies", "CollectionFolder");
        pane.views = vec![lib.clone(), other];
        pane.start_jump(lib, false, None, None, &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "lib".to_owned(),
            items: vec![],
        });
        assert!(pane.tree_scope.is_some());
        pane.collapse_tree(&ctx).unwrap();
        assert!(pane.tree_scope.is_none());
        assert_eq!(
            pane.tree.iter().filter(|node| node.depth == 0).count(),
            2,
            "the full tree is back: {:?}",
            pane.tree
        );
    }

    /// A series result opens on the season the query named (`S02`), with
    /// its first season highlighted otherwise.
    #[test]
    fn a_series_jump_goes_to_the_named_season() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        pane.views = vec![jf_item("lib", "TV", "CollectionFolder")];
        let series = jf_item("series", "Futurama", "Series");
        let s1 = jf_item("s1", "Season 1", "Season");
        let mut s2 = jf_item("s2", "Season 2", "Season");
        s2.index_number = Some(2);

        let library = pane.views[0].clone();
        pane.start_jump(series.clone(), false, Some(2), None, &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "series".to_owned(),
            items: vec![library],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![s1, s2],
        });

        assert!(pane.jump.is_none());
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("series"),
                   "a series opens INTO the series: its seasons are the right pane");
        assert_eq!(pane.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
                   ["s1", "s2"]);
        assert_eq!(pane.item_list.selected(), Some(1), "season 2 is highlighted");
    }

    /// `Show S02E03` on a series result goes one level deeper: the season is
    /// resolved and the episode inside it is highlighted (no playback).
    #[test]
    fn a_series_jump_with_an_episode_hint_opens_the_episode() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        pane.views = vec![jf_item("lib", "TV", "CollectionFolder")];
        let series = jf_item("series", "Futurama", "Series");
        let s1 = jf_item("s1", "Season 1", "Season");
        let mut s2 = jf_item("s2", "Season 2", "Season");
        s2.index_number = Some(2);
        let mut e1 = jf_item("e1", "One", "Episode");
        e1.index_number = Some(1);
        let mut e3 = jf_item("e3", "Three", "Episode");
        e3.index_number = Some(3);

        let library = pane.views[0].clone();
        pane.start_jump(series.clone(), false, Some(2), Some(3), &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "series".to_owned(),
            items: vec![library],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![s1, s2],
        });
        // The retargeted jump asked for season 2's episodes.
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "s2".to_owned(),
            items: vec![e1, e3],
        });

        assert!(pane.jump.is_none());
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("s2"));
        assert_eq!(pane.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
                   ["e1", "e3"]);
        assert_eq!(pane.item_list.selected(), Some(1), "episode 3 is highlighted");
    }

    /// Watched items carry a check in the browser's item rows (the app's
    /// own view of what the server recorded).
    #[test]
    fn watched_items_show_a_check_row() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        pane.mode = JellyfinTabMode::Libraries;
        let mut watched = jf_item("e1", "Space Pilot 3000", "Episode");
        watched.played = true;
        pane.items = vec![watched, jf_item("e2", "The Series Has Landed", "Episode")];
        pane.item_list.select(Some(0));
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("✓ Space Pilot 3000"), "{text}");
    }

    /// A jump that lands before the views fetch has delivered the library
    /// synthesizes the library root, so the expanded path hangs off a
    /// visible tree row instead of an orphaned `folder:` key.
    #[test]
    fn a_jump_reaches_a_library_the_views_fetch_has_not_delivered() {
        let (mut pane, ctx, _work_rx) = pane_with_server();
        assert!(pane.views.is_empty());
        let library = jf_item("lib", "Movies", "CollectionFolder");
        let movie = jf_item("m1", "A Movie", "Movie");
        pane.start_jump(movie.clone(), false, None, None, &ctx).unwrap();
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "m1".to_owned(),
            items: vec![library.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![movie.clone(), jf_item("m2", "Another Movie", "Movie")],
        });
        assert!(pane.views.iter().any(|view| view.id == "lib"),
                "the library root is synthesized: {:?}", pane.views);
        assert!(pane.expanded.contains("view:lib"),
                "the library expands under its view key: {:?}", pane.expanded);
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("lib"));
        assert_eq!(pane.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
                   ["m1", "m2"]);
        assert_eq!(pane.item_list.selected(), Some(0), "the movie is selected");
        // Movies have no tree row of their own: the highlight falls back to
        // the library node.
        assert_eq!(pane.tree_list.selected(), Some(0));
    }

    /// Jellyfin answers the ancestor chain with its own `UserRootFolder`
    /// above the libraries; it has no tree row, so the jump drops it and
    /// starts loading at the library.
    #[test]
    fn the_jellyfin_root_folder_is_not_part_of_the_jump() {
        let (mut pane, ctx, work_rx) = pane_with_server();
        pane.views = vec![jf_item("lib", "Anime", "CollectionFolder")];
        let season = jf_item("season1", "Season 1", "Season");
        let series = jf_item("series", "Overgeared", "Series");
        let target = jf_item("ep1", "Legendary Class", "Episode");
        let root = jf_item("root", "Media Folders", "UserRootFolder");
        let library = pane.views[0].clone();
        pane.start_jump(target, false, None, None, &ctx).unwrap();
        // Drain the ancestors request, then answer nearest-first with the
        // root folder last (the live server's order).
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::FetchJellyfinAncestors { .. })
        ));
        feed(&mut pane, &ctx, JF_ANCESTORS, JellyfinResult::Ancestors {
            item_id: "ep1".to_owned(),
            items: vec![season.clone(), series.clone(), library, root.clone()],
        });
        // The first children fetch is the library's, never the root's.
        assert!(matches!(
            work_rx.try_recv(),
            Ok(WorkRequest::FetchJellyfinFolder { ref parent_id }) if parent_id == "lib"
        ));
        assert!(!pane.folders.contains_key("root"), "the root is never fetched");
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "lib".to_owned(),
            items: vec![series.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "series".to_owned(),
            items: vec![season.clone()],
        });
        feed(&mut pane, &ctx, JF_FOLDER, JellyfinResult::Children {
            parent_id: "season1".to_owned(),
            items: vec![jf_item("ep1", "Legendary Class", "Episode")],
        });
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("season1"));
        assert!(!pane.expanded.contains("folder:root"), "{:?}", pane.expanded);
    }

    /// One card of a row: the leftmost (`prefer_max`) or the rightmost.
    fn row_card(pane: &JellyfinPane, row: usize, prefer_max: usize) -> home::HomeCard {
        let cards: Vec<home::HomeCard> = pane
            .home_layout
            .cards
            .iter()
            .filter(|card| card.row == home::HomeRowId::Section(row))
            .cloned()
            .collect();
        assert!(!cards.is_empty(), "row {row} has cards");
        if prefer_max == 0 {
            cards.into_iter().max_by_key(|c| c.rect.right()).unwrap()
        } else {
            cards.into_iter().min_by_key(|c| c.rect.x).unwrap()
        }
    }

    /// A modal is drawn over the cards in the frame buffer, but the carousel
    /// images are terminal-side overlays placed *after* the flush — while a
    /// modal is up the wall must stay empty or the artwork paints over it.
    #[test]
    fn a_modal_keeps_the_carousel_images_off_the_screen() {
        let (mut pane, ctx, _work_rx) = kitty_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text(&mut pane, &ctx);
        assert!(
            !pane.wall.layout().is_empty(),
            "without a modal the cards show their images"
        );
        let mut opened = crate::ui::UiEvent::ModalOpened;
        pane.on_event(&mut opened, true, &ctx).unwrap();
        assert!(pane.is_modal_open);
        let _ = render_text(&mut pane, &ctx);
        assert!(
            pane.wall.layout().is_empty(),
            "no image is re-placed on the frame the modal is drawn in"
        );
        let mut closed = crate::ui::UiEvent::ModalClosed;
        pane.on_event(&mut closed, true, &ctx).unwrap();
        let _ = render_text(&mut pane, &ctx);
        assert!(
            !pane.wall.layout().is_empty(),
            "closing the modal brings the images back"
        );
    }

    fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        MouseEvent {
            kind,
            x,
            y,
            modifiers: ratatui::crossterm::event::KeyModifiers::NONE,
        }
    }

    /// The wheel moves between carousels exactly like Up/Down (it no longer
    /// scrolls the row sideways).
    #[test]
    fn the_mouse_wheel_moves_between_carousels() {
        let (mut pane, ctx) = home_rig();
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text(&mut pane, &ctx);
        let scroll = mouse(MouseEventKind::ScrollDown, 5, 5);
        assert!(pane.handle_home_mouse(scroll, &ctx).unwrap());
        assert_eq!(pane.home_focus, home::HomeFocus::Section(1),
                   "the wheel focuses the next carousel");
        assert_eq!(pane.home_sections[0].cursor, 0, "the row itself did not scroll");
        let scroll = mouse(MouseEventKind::ScrollUp, 5, 5);
        assert!(pane.handle_home_mouse(scroll, &ctx).unwrap());
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        assert_eq!(pane.home_sections[0].cursor, 0);
    }

    /// A carousel that can scroll right shows only a right arrow; once the
    /// cursor reaches the end, only the left arrow remains. The arrows are
    /// drawn on the cards' middle row (not on the header) in the row's own
    /// margins: the blank space after the last card is the right arrow, and a
    /// left gutter appears once the row has scrolled back. Clicking one
    /// focuses that row and steps it.
    #[test]
    fn carousel_arrows_sit_in_the_row_margins() {
        let (mut pane, ctx) = home_rig();
        let resume: Vec<crate::jellyfin::JfItem> = (0..5)
            .map(|i| {
                let mut item = jf_item(&format!("ep{i}"), &format!("Ep {i}"), "Episode");
                item.series_name = Some("Show".to_owned());
                item.season_number = Some(1);
                item.index_number = Some(i + 1);
                item
            })
            .collect();
        pane.home = crate::jellyfin::JfHome {
            continue_watching: resume,
            // Two more sections keep the rows short, so three preview cards
            // fit and the five Continue Watching cards overflow.
            next_up: vec![jf_item("n1", "Next One", "Episode")],
            recently_added: vec![jf_item("r1", "Recent One", "Series")],
            library_latest: Vec::new(),
        };
        pane.rebuild_home_sections();
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains('›'), "a row with more cards shows the right arrow: {text}");
        assert!(!text.contains('‹'), "nothing to the left yet: {text}");
        assert!(text.contains("⟨1-3 of 5⟩"), "the visible range: {text}");
        let (right, row_id, _dir) = pane
            .home_layout
            .arrows
            .iter()
            .find(|(_, row, dir)| {
                *row == home::HomeRowId::Section(0) && *dir == 1
            })
            .copied()
            .expect("right arrow for the first carousel");
        assert_eq!(row_id, home::HomeRowId::Section(0));
        let first = row_card(&pane, 0, usize::MAX);
        let last = row_card(&pane, 0, 0);
        assert_eq!(first.rect.x, 0, "nothing to the left: the cards start at the row's edge");
        assert_eq!(
            right.x,
            last.rect.right(),
            "the right arrow starts in the blank space after the last card"
        );
        assert_eq!(
            right.y, first.rect.y,
            "the arrow's strip starts at the cards' top"
        );
        assert_eq!(
            right.height,
            pane.home_layout.sections[0].1.height,
            "the whole blank strip is the target"
        );
        // The glyph is drawn on the cards' middle row, not on the header.
        let lines: Vec<&str> = text.lines().collect();
        let glyph_x = usize::from(right.right() - 1);
        let mid_y = usize::from(first.rect.y + (first.rect.height - 2) / 2);
        assert_eq!(
            lines[mid_y].chars().nth(glyph_x),
            Some('›'),
            "the right arrow is on the cards' middle row"
        );
        let header_y = usize::from(pane.home_layout.sections[0].0.y);
        assert_ne!(
            lines[header_y].chars().nth(glyph_x),
            Some('›'),
            "nothing is drawn on the header row"
        );
        // Clicking the blank space pages the row: the selection lands on the
        // card that was hidden (`ep3`), not on the next visible one.
        assert!(pane
            .handle_home_mouse(
                mouse(MouseEventKind::LeftClick, right.x + 1, right.y),
                &ctx
            )
            .unwrap());
        assert_eq!(
            pane.home_selected_item().map(|i| i.id),
            Some("ep3".to_owned()),
            "one click reveals the next hidden card and selects it"
        );
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("⟨2-4 of 5⟩"), "the row progressed one card: {text}");
        // Page once more to the end: only the left arrow remains.
        let (next_right, _, _) = pane
            .home_layout
            .arrows
            .iter()
            .find(|(_, r, d)| *r == home::HomeRowId::Section(0) && *d == 1)
            .copied()
            .expect("the right arrow is still there after one page");
        assert!(pane
            .handle_home_mouse(
                mouse(MouseEventKind::LeftClick, next_right.x + 1, next_right.y),
                &ctx
            )
            .unwrap());
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep4".to_owned()));
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("⟨3-5 of 5⟩"), "the last window: {text}");
        let left = pane
            .home_layout
            .arrows
            .iter()
            .find(|(_, r, d)| *r == home::HomeRowId::Section(0) && *d == -1)
            .copied()
            .expect("a left arrow once the row is scrolled");
        let (left_rect, _, _) = left;
        assert_eq!(left_rect.x, 0, "the left gutter is the row's first columns");
        assert_eq!(left_rect.width, 2, "the arrow plus one blank column");
        let scrolled = render_text(&mut pane, &ctx);
        let scrolled_lines: Vec<&str> = scrolled.lines().collect();
        assert_eq!(
            scrolled_lines[usize::from(left_rect.y + (left_rect.height - 2) / 2)]
                .chars()
                .next(),
            Some('‹'),
            "the left arrow is centred on the cards"
        );
        assert_eq!(
            row_card(&pane, 0, usize::MAX).rect.x,
            2,
            "the cards shift right to make room for the left arrow"
        );
        assert!(
            !pane.home_layout
                .arrows
                .iter()
                .any(|(_, r, d)| *r == home::HomeRowId::Section(0) && *d == 1),
            "no right arrow at the end"
        );
        // The clicked arrow also focused its row.
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        // The left arrow pages back the same way: the row scrolls one card
        // and the selection lands on the card revealed at the left.
        assert!(pane
            .handle_home_mouse(
                mouse(MouseEventKind::LeftClick, left_rect.x, left_rect.y),
                &ctx
            )
            .unwrap());
        assert_eq!(
            pane.home_selected_item().map(|i| i.id),
            Some("ep1".to_owned()),
            "the left arrow reveals the card that scrolled off and selects it"
        );
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("⟨2-4 of 5⟩"), "back one window: {text}");
    }

    /// The Libraries carousel sits above the shelves but is hidden (a hint
    /// line) until the user presses Up from the first Continue Watching
    /// card; Down then returns to the shelves.
    #[test]
    fn up_from_the_first_carousel_reveals_the_libraries_row() {
        let (mut pane, mut ctx) = home_rig();
        pane.views = vec![
            jf_item("lib1", "Anime", "CollectionFolder"),
            jf_item("lib2", "Movies", "CollectionFolder"),
        ];
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("↑ Libraries"), "the hidden row leaves a hint: {text}");
        assert!(
            !pane.home_layout.cards.iter().any(|card| card.row == home::HomeRowId::Libraries),
            "no library cards while hidden"
        );
        // The default focus is the first Continue Watching card.
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        assert_eq!(pane.home_selected_item().map(|i| i.id), Some("ep1".to_owned()));

        // Up reveals the row and lands on it.
        let mut event = crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
            crate::shared::keys::Actions::Directories(
                crate::config::keys::DirectoriesActions::FolderUp,
            ),
        ]));
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Libraries);
        assert!(pane.home_libraries_revealed);
        let text = render_text(&mut pane, &ctx);
        assert!(!text.contains("↑ Libraries"), "the hint is replaced by the row: {text}");
        let library_cards: Vec<&home::HomeCard> = pane
            .home_layout
            .cards
            .iter()
            .filter(|card| card.row == home::HomeRowId::Libraries)
            .collect();
        assert_eq!(library_cards.len(), 2, "one card per library");
        // Its cards are the libraries, and the first one starts at the top.
        assert_eq!(
            pane.home_selected_item().map(|i| i.name),
            Some("Anime".to_owned())
        );
        assert!(
            library_cards[0].rect.y
                < pane
                    .home_layout
                    .cards
                    .iter()
                    .filter(|card| card.row == home::HomeRowId::Section(0))
                    .map(|card| card.rect.y)
                    .min()
                    .unwrap_or(u16::MAX),
            "the Libraries row is above Continue Watching"
        );

        // Down returns to the shelves.
        let mut event = crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
            crate::shared::keys::Actions::Directories(
                crate::config::keys::DirectoriesActions::FolderDown,
            ),
        ]));
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));
        // Down also collapses the row again (it becomes the hint).
        assert!(!pane.home_libraries_revealed);
        let text = render_text(&mut pane, &ctx);
        assert!(text.contains("↑ Libraries"), "the hint is back: {text}");
        assert!(
            !pane
                .home_layout
                .cards
                .iter()
                .any(|card| card.row == home::HomeRowId::Libraries),
            "no library cards after collapsing"
        );
    }

    /// A taller TUI must not inflate the posters: the cards stop at a
    /// comfortable image height and the spare rows go to the Libraries
    /// carousel (2026-10-28, two user screenshots of the same page at
    /// different terminal heights).
    #[test]
    fn a_tall_page_reveals_the_libraries_row_instead_of_growing_the_posters() {
        let (mut pane, ctx) = home_rig();
        pane.views = vec![
            jf_item("lib1", "Anime", "CollectionFolder"),
            jf_item("lib2", "Movies", "CollectionFolder"),
        ];
        pane.home = shelf_home();
        pane.rebuild_home_sections();

        // 40 rows: too short for a fourth row, so the dim hint stands in.
        let short = render_text_at(&mut pane, &ctx, 100, 40);
        assert!(short.contains("↑ Libraries"), "the hint is the short page's row: {short}");
        assert!(
            !pane
                .home_layout
                .cards
                .iter()
                .any(|card| card.row == home::HomeRowId::Libraries),
            "no library cards on a short page"
        );

        // 100 rows: the Libraries row takes the spare height instead of the
        // cards growing to fill it.
        let tall = render_text_at(&mut pane, &ctx, 100, 100);
        assert!(!tall.contains("↑ Libraries"), "the row replaces its hint: {tall}");
        assert_eq!(
            pane.home_layout.sections.len(),
            4,
            "Libraries + three shelves are on the page"
        );
        assert!(
            pane.home_layout
                .cards
                .iter()
                .any(|card| card.row == home::HomeRowId::Libraries),
            "the library cards are painted"
        );
        let tall_h = pane
            .home_layout
            .cards
            .iter()
            .find(|card| card.row == home::HomeRowId::Section(0))
            .expect("a Continue Watching card")
            .image
            .height;
        assert_eq!(
            tall_h,
            home::MAX_IMAGE_H,
            "the posters stay at their preferred size"
        );

        // More rows still do not grow the posters.
        let _ = render_text_at(&mut pane, &ctx, 100, 140);
        let taller_h = pane
            .home_layout
            .cards
            .iter()
            .find(|card| card.row == home::HomeRowId::Section(0))
            .expect("a Continue Watching card")
            .image
            .height;
        assert_eq!(taller_h, tall_h, "extra height does not inflate the cards");

        // Shrinking the terminal gives the row back to its hint.
        let short_again = render_text_at(&mut pane, &ctx, 100, 40);
        assert!(short_again.contains("↑ Libraries"), "the hint is back: {short_again}");
        assert!(
            !pane
                .home_layout
                .cards
                .iter()
                .any(|card| card.row == home::HomeRowId::Libraries),
            "the library cards are gone again"
        );
    }

    /// When the page is tall enough to paint the Libraries carousel, Up
    /// steps onto it: the row is already shown, so there is no hidden row
    /// to reveal.
    #[test]
    fn up_reaches_the_libraries_row_when_the_page_shows_it() {
        let (mut pane, mut ctx) = home_rig();
        pane.views = vec![jf_item("lib1", "Anime", "CollectionFolder")];
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        let _ = render_text_at(&mut pane, &ctx, 100, 100);
        assert_eq!(pane.home_focus, home::HomeFocus::Section(0));

        let mut event = crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
            crate::shared::keys::Actions::Directories(
                crate::config::keys::DirectoriesActions::FolderUp,
            ),
        ]));
        pane.handle_home_action(&mut event, &mut ctx).unwrap();
        assert_eq!(pane.home_focus, home::HomeFocus::Libraries);
        assert_eq!(
            pane.home_selected_item().map(|i| i.id),
            Some("lib1".to_owned())
        );
        assert!(
            !pane.home_libraries_revealed,
            "the row was never hidden, so Up does not reveal it"
        );
    }

    /// Every library with recent items gets its own row after the
    /// server-wide Recently Added shelf; an empty library gets none. When
    /// the whole page still fits, the Libraries row stays visible (a small
    /// server needs no Up hint).
    #[test]
    fn each_library_gets_its_own_recently_added_row() {
        let (mut pane, mut ctx) = home_rig();
        pane.views = vec![
            jf_item("lib1", "Anime", "CollectionFolder"),
            jf_item("lib2", "Movies", "CollectionFolder"),
            jf_item("lib3", "Documentaries", "CollectionFolder"),
        ];
        pane.home = shelf_home();
        pane.home.library_latest = vec![
            library_strip("Anime", &["a1", "a2", "a3"]),
            library_strip("Movies", &["m1", "m2"]),
            library_strip("Documentaries", &[]),
        ];
        pane.rebuild_home_sections();
        let rows: Vec<(&str, bool)> = pane
            .home_sections
            .iter()
            .map(|section| (section.title.as_str(), section.library_strip))
            .collect();
        assert_eq!(
            rows,
            [
                ("Continue Watching", false),
                ("Next Up", false),
                ("Recently Added", false),
                ("Recently Added in Anime", true),
                ("Recently Added in Movies", true),
            ],
            "the empty library contributes no row"
        );

        let text = render_text_at(&mut pane, &ctx, 100, 120);
        assert!(pane.home_libraries_fits, "the whole page fits at this size");
        assert!(!text.contains("↑ Libraries"), "the Libraries row is on the page");
        assert!(text.contains("Recently Added in Anime"), "{text}");
        assert!(text.contains("Recently Added in Movies"), "{text}");
        assert!(!text.contains("Recently Added in Documentaries"), "{text}");
        let anime_cards: Vec<&home::HomeCard> = pane
            .home_layout
            .cards
            .iter()
            .filter(|card| card.row == home::HomeRowId::Section(3))
            .collect();
        assert!(!anime_cards.is_empty(), "the Anime strip painted its cards");
        assert!(
            anime_cards
                .iter()
                .all(|card| card.image.height == home::MAX_IMAGE_H),
            "library strips share the poster size"
        );

        // Down from the server-wide Recently Added row enters the first
        // library strip.
        for _ in 0..3 {
            let mut event = crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
                crate::shared::keys::Actions::Directories(
                    crate::config::keys::DirectoriesActions::FolderDown,
                ),
            ]));
            pane.handle_home_action(&mut event, &mut ctx).unwrap();
        }
        assert_eq!(pane.home_focus, home::HomeFocus::Section(3));
        assert_eq!(
            pane.home_selected_item().map(|i| i.id),
            Some("a1".to_owned())
        );
    }

    /// More library rows than fit: the page scrolls at the preferred poster
    /// size instead of shrinking every card, and the Libraries row goes
    /// back to its Up hint.
    #[test]
    fn many_library_rows_scroll_and_hide_the_libraries_row() {
        let (mut pane, mut ctx) = home_rig();
        pane.views = (0..8)
            .map(|i| {
                jf_item(
                    &format!("lib{i}"),
                    &format!("Library {i}"),
                    "CollectionFolder",
                )
            })
            .collect();
        pane.home = shelf_home();
        pane.home.library_latest = (0..8)
            .map(|i| library_strip(&format!("Library {i}"), &["latest"]))
            .collect();
        pane.rebuild_home_sections();
        assert_eq!(pane.home_sections.len(), 11);

        let text = render_text_at(&mut pane, &ctx, 100, 100);
        assert!(!pane.home_libraries_fits, "eleven rows do not fit the page");
        assert!(text.contains("↑ Libraries"), "the carousel is the hint again: {text}");
        assert!(
            pane.home_layout
                .cards
                .iter()
                .all(|card| card.image.height == home::MAX_IMAGE_H),
            "extra rows do not shrink the cards"
        );
        assert_eq!(
            pane.home_layout.sections.len(),
            6,
            "only the rows that fit are painted"
        );
        assert!(text.contains("Continue Watching"), "{text}");
        assert!(
            text.contains("Recently Added in Library 2"),
            "the first library rows are visible: {text}"
        );
        assert!(
            !text.contains("Recently Added in Library 7"),
            "the last rows need scrolling: {text}"
        );

        // Down to the last row scrolls it into view.
        for _ in 0..10 {
            let mut event = crate::shared::keys::ActionEvent::from(std::sync::Arc::new(vec![
                crate::shared::keys::Actions::Directories(
                    crate::config::keys::DirectoriesActions::FolderDown,
                ),
            ]));
            pane.handle_home_action(&mut event, &mut ctx).unwrap();
        }
        assert_eq!(pane.home_focus, home::HomeFocus::Section(10));
        let text = render_text_at(&mut pane, &ctx, 100, 100);
        assert!(pane.home_section_scroll > 0, "the page scrolled");
        assert!(text.contains("Recently Added in Library 7"), "{text}");
        assert!(
            !text.contains("Continue Watching"),
            "the first row scrolled away: {text}"
        );
    }

    /// Activating a library card opens the library tree on that library.
    #[test]
    fn selecting_a_library_opens_the_tree_on_it() {
        let (mut pane, ctx) = home_rig();
        let anime = jf_item("lib1", "Anime", "CollectionFolder");
        let series = jf_item("series1", "Overgeared", "Series");
        pane.views = vec![anime.clone()];
        pane.folders.insert("lib1".to_owned(), vec![series.clone()]);
        pane.home = shelf_home();
        pane.rebuild_home_sections();
        pane.home_libraries_revealed = true;
        pane.home_focus = home::HomeFocus::Libraries;
        let _ = render_text(&mut pane, &ctx);
        pane.home_activate(&ctx).unwrap();
        assert_eq!(pane.mode, JellyfinTabMode::Libraries);
        assert_eq!(pane.selected.as_ref().map(JfNodeKind::id), Some("lib1"));
        assert!(pane.expanded.contains("view:lib1"), "the library is expanded");
        assert_eq!(
            pane.items.iter().map(|i| i.id.as_str()).collect::<Vec<_>>(),
            ["series1"],
            "the right pane lists the library's children"
        );
    }
