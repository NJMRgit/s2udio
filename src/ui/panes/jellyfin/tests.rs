    use super::*;
    use crate::{
        MpdQueryResult,
        jellyfin::JellyfinResult,
        shared::events::WorkRequest,
    };

    fn render_text(pane: &mut JellyfinPane, ctx: &crate::ctx::Ctx) -> String {
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                pane.render(frame, ratatui::prelude::Rect::new(0, 0, 100, 40), ctx).unwrap()
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        (0..40u16)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n")
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
