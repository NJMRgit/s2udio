#![allow(clippy::unwrap_used)]

use std::{
    collections::HashMap,
    sync::{
        LazyLock,
        atomic::{AtomicU32, Ordering},
    },
    time::Duration,
};

use rstest::{fixture, rstest};

use crate::{
    ctx::Ctx,
    mpd::commands::Song,
    tests::fixtures::ctx,
    ui::{
        browser::BrowserPane,
        dir_or_song::DirOrSong,
        panes::{Pane, playlists::PlaylistsPane},
    },
};

mod on_idle_event {
    use super::*;
    use crate::{
        ctx::Ctx,
        shared::mpd_query::MpdQueryResult,
        ui::panes::playlists::{INIT, REINIT},
    };

    mod browsing_playlists {

        use super::*;

        #[rstest]
        fn selects_the_same_playlist_by_name(mut screen: PlaylistsPane, ctx: Ctx) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            let current = screen.stack.current_mut();
            current.select_idx(1, 0);
            assert_eq!(current.selected(), Some(dir("pl2")).as_ref());

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong { data: vec![dir("pl2"), dir("pl4")], path: None },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected(), Some(dir("pl2")).as_ref());
        }

        #[rstest]
        fn selects_the_same_index_when_playlist_not_found_after_refresh(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 2);
        }

        #[rstest]
        fn selects_the_last_playlist_when_last_was_selected_and_removed(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(3, 0);

            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong { data: vec![dir("pl1"), dir("pl2")], path: None },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 1);
        }

        #[rstest]
        fn selects_the_first_playlist_when_first_was_selected_and_removed(
            mut screen: PlaylistsPane,
            ctx: Ctx,
        ) {
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(0, 0);
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong { data: vec![dir("pl3"), dir("pl4")], path: None },
                    true,
                    &ctx,
                )
                .unwrap();

            assert_eq!(screen.stack.current().selected_with_idx().unwrap().0, 0);
        }
    }

    mod browsing_songs {
        use crossbeam::channel::{Receiver, Sender};

        use super::*;
        use crate::{
            shared::events::{AppEvent, ClientRequest, WorkRequest},
            tests::fixtures::{app_event_channel, client_request_channel, work_request_channel},
            ui::panes::playlists::FETCH_DATA,
        };

        #[rstest]
        fn selects_the_same_playlist_and_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            // init playlists
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            // select third playlist ind init its songs
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            // select third song - s3
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );

            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s2"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            // send in new songs without s1
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx.send(MpdQueryResult::Any(Box::new(new_songs2))).unwrap();
                }
            });
            // trigger reinit of playlists without pl1
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(screen.stack.previous().and_then(|p| p.selected()), Some(&dir("pl3")));
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_last_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s2")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx.send(MpdQueryResult::Any(Box::new(new_songs2))).unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(screen.stack.previous().and_then(|p| p.selected()), Some(&dir("pl3")));

            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_first_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[2].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx.send(MpdQueryResult::Any(Box::new(new_songs2))).unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(screen.stack.previous().and_then(|p| p.selected()), Some(&dir("pl3")));
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[0].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_and_song_idx(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(1, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[1].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx.send(MpdQueryResult::Any(Box::new(new_songs2))).unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(screen.stack.previous().and_then(|p| p.selected()), Some(&dir("pl3")));
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }

        #[rstest]
        fn selects_the_same_playlist_idx_and_last_song(
            mut screen: PlaylistsPane,
            app_event_channel: (Sender<AppEvent>, Receiver<AppEvent>),
            work_request_channel: (Sender<WorkRequest>, Receiver<WorkRequest>),
            client_request_channel: (Sender<ClientRequest>, Receiver<ClientRequest>),
        ) {
            let rx = client_request_channel.1.clone();
            let ctx = ctx(app_event_channel, work_request_channel, client_request_channel);
            let initial_songs = [song("s1"), song("s2"), song("s3"), song("s4")];
            let initial_playlists = vec![dir("pl1"), dir("pl2"), dir("pl3"), dir("pl4")];
            screen
                .on_query_finished(
                    INIT,
                    MpdQueryResult::DirOrSong { data: initial_playlists, path: None },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(2, 0);
            screen.stack_mut().enter();
            screen
                .on_query_finished(
                    FETCH_DATA,
                    MpdQueryResult::DirOrSong {
                        data: initial_songs.iter().cloned().map(DirOrSong::Song).collect(),
                        path: Some("pl3".into()),
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            screen.stack.current_mut().select_idx(1, 0);
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(initial_songs[1].clone()))
            );
            while rx.recv_timeout(Duration::from_millis(1)).is_ok() {}

            // then
            let rx2 = rx.clone();
            let new_songs = vec![song("s1"), song("s3"), song("s4")];
            let new_songs2 = new_songs.clone();
            std::thread::spawn(move || {
                let req = rx2.recv().unwrap();
                if let ClientRequest::QuerySync(qry) = req {
                    qry.tx.send(MpdQueryResult::Any(Box::new(new_songs2))).unwrap();
                }
            });
            screen
                .on_query_finished(
                    REINIT,
                    MpdQueryResult::DirOrSong {
                        data: vec![dir("pl1"), dir("pl2"), dir("pl4")],
                        path: None,
                    },
                    true,
                    &ctx,
                )
                .unwrap();
            assert_eq!(screen.stack.previous().and_then(|p| p.selected()), Some(&dir("pl4")));
            assert_eq!(
                screen.stack.current().selected(),
                Some(&DirOrSong::Song(new_songs[1].clone()))
            );
        }
    }
}

static LAST_ID: AtomicU32 = AtomicU32::new(1);
static NOW: LazyLock<chrono::DateTime<chrono::Utc>> = LazyLock::new(chrono::Utc::now);

pub fn new_id() -> u32 {
    LAST_ID.fetch_add(1, Ordering::Relaxed)
}
fn song(name: &str) -> Song {
    Song {
        id: new_id(),
        file: name.to_string(),
        duration: Some(Duration::from_secs(1)),
        metadata: HashMap::new(),
        last_modified: *NOW,
        added: None,
    }
}

fn dir(name: &str) -> DirOrSong {
    DirOrSong::Dir {
        name: name.to_string(),
        full_path: name.to_string(),
        last_modified: *NOW,
        playlist: false,
    }
}

#[fixture]
fn screen(ctx: Ctx) -> PlaylistsPane {
    let mut screen = PlaylistsPane::new(&ctx);
    screen.before_show(&ctx).unwrap();
    screen
}

/// Playlist audio/video classification: the ♪ / ▶ prefixes, and the
/// stream display helpers (cached title instead of a raw URL).
mod playlist_kinds {
    use super::*;
    use crate::{
        MpdQueryResult,
        shared::ytdlp::YtStreamInfo,
        ui::panes::playlists::{PlaylistKind, is_video_uri, playlist_items, stream_display_title},
    };
    use ratatui::prelude::Rect;
    use std::collections::BTreeSet;

    #[test]
    fn audio_entries_classify_as_audio() {
        let songs = vec![song("music/a.flac"), song("music/b.mp3")];
        assert_eq!(PlaylistKind::of(&songs), PlaylistKind::Audio);
    }

    #[test]
    fn a_video_file_classifies_the_playlist_as_video() {
        let songs = vec![song("videos/clip.mkv")];
        assert_eq!(PlaylistKind::of(&songs), PlaylistKind::Video);
    }

    #[test]
    fn a_video_url_classifies_the_playlist_as_video() {
        // Query strings are stripped before the extension check.
        assert!(is_video_uri("https://example.com/movie.mp4?x=1"));
        let songs = vec![song("https://example.com/movie.mp4?x=1")];
        assert_eq!(PlaylistKind::of(&songs), PlaylistKind::Video);
    }

    #[test]
    fn youtube_links_classify_as_audio() {
        let songs = vec![song("https://youtu.be/abc123")];
        assert_eq!(PlaylistKind::of(&songs), PlaylistKind::Audio);
    }

    #[test]
    fn audio_files_and_urls_are_not_video() {
        assert!(!is_video_uri("music/a.flac"));
        assert!(!is_video_uri("https://example.com/song.mp3"));
        assert!(!is_video_uri("https://youtu.be/abc"));
    }

    #[test]
    fn prefixes_match_the_spec() {
        assert_eq!(PlaylistKind::Audio.prefix(), "♪ ");
        assert_eq!(PlaylistKind::Video.prefix(), "▶  ");
    }

    #[test]
    fn stream_title_comes_from_the_cached_info() {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let info = YtStreamInfo {
            url: "https://rr4.example/audio.m4a".to_owned(),
            original_url: "https://youtu.be/abc".to_owned(),
            title: "Some Mix".to_owned(),
            ..Default::default()
        };
        ctx.yt_info.borrow_mut().insert("https://rr4.example/audio.m4a".to_owned(), info.clone());
        // The resolved URL (the queue/playlist URI) resolves by exact key.
        assert_eq!(
            stream_display_title(&ctx, "https://rr4.example/audio.m4a"),
            Some("Some Mix".to_owned())
        );
        // The original link (a playlist may hold either) resolves by a
        // matching `original_url`.
        assert_eq!(
            stream_display_title(&ctx, "https://youtu.be/abc"),
            Some("Some Mix".to_owned())
        );
        // Uncached streams and local files have nothing to show.
        assert_eq!(stream_display_title(&ctx, "https://other.example/x"), None);
        assert_eq!(stream_display_title(&ctx, "music/a.flac"), None);
    }

    /// Reconstruct the rendered lines of a buffer for text assertions.
    fn buffer_rows(buffer: &ratatui::buffer::Buffer) -> Vec<String> {
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn playlist_rows_show_the_kind_prefix() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = PlaylistsPane::new(&ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::playlist_name_only("mix".to_owned()),
                    DirOrSong::playlist_name_only("films".to_owned()),
                ],
                path: None,
            },
            true,
            &ctx,
        )
        .unwrap();
        pane.playlist_kinds.insert("mix".to_owned(), PlaylistKind::Audio);
        pane.playlist_kinds.insert("films".to_owned(), PlaylistKind::Video);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), &ctx).unwrap()).unwrap();
        let rows = buffer_rows(terminal.backend().buffer());
        assert!(rows.iter().any(|r| r.contains("♪ mix")), "audio prefix missing: {rows:?}");
        assert!(rows.iter().any(|r| r.contains("▶") && r.contains("films")), "video prefix missing: {rows:?}");
    }

    /// At the root the right pane lists every playlist (the root's
    /// children, like the MPD pane's right pane at the root) instead of
    /// the songs preview of the highlighted playlist.
    #[test]
    fn right_pane_lists_playlists_at_the_root() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = PlaylistsPane::new(&ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::playlist_name_only("mix".to_owned()),
                    DirOrSong::playlist_name_only("films".to_owned()),
                ],
                path: None,
            },
            true,
            &ctx,
        )
        .unwrap();
        pane.playlist_kinds.insert("mix".to_owned(), PlaylistKind::Audio);
        pane.playlist_kinds.insert("films".to_owned(), PlaylistKind::Video);

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), &ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        // On an 80-col TUI the left pane is hidden (≤ 120 cols), so the
        // right pane spans the whole area: its block is titled Playlists,
        // not Songs, and the rows start at the inner x=1 border offset.
        let right_text: String = (0..24u16)
            .map(|y| (1..80u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            right_text.contains("♪ mix") && right_text.contains("films"),
            "right pane lists the playlists at the root: {right_text}"
        );
        assert!(!right_text.contains("Songs"), "right pane title is Playlists at the root: {right_text}");
    }

    #[test]
    fn playlist_rows_default_to_audio_for_unknown_kinds() {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // No kinds loaded yet (background classification pending): the row
        // still renders with the audio prefix rather than nothing.
        let items = playlist_items(
            &[DirOrSong::playlist_name_only("legacy".to_owned())],
            &BTreeSet::new(),
            None,
            &ctx,
            &HashMap::new(),
        );
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn stream_songs_show_the_cached_title_in_the_stream_color() {
        use ratatui::style::Color;

        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                original_url: "https://youtu.be/abc".to_owned(),
                title: "Some Mix".to_owned(),
                ..Default::default()
            },
        );
        let mut pane = PlaylistsPane::new(&ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::playlist_name_only("mix".to_owned())],
                path: None,
            },
            true,
            &ctx,
        )
        .unwrap();
        // Enter the playlist and load its songs: a stream (cached info)
        // and a local file.
        pane.stack.current_mut().select_idx(0, 0);
        pane.stack_mut().enter();
        pane.on_query_finished(
            super::super::FETCH_DATA,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::Song(song("https://rr4.example/audio.m4a")),
                    DirOrSong::Song(song("music/local.flac")),
                ],
                path: Some("mix".into()),
            },
            true,
            &ctx,
        )
        .unwrap();

        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), &ctx).unwrap()).unwrap();
        let buffer = terminal.backend().buffer();
        let rows = buffer_rows(buffer);
        assert!(rows.iter().any(|r| r.contains("Some Mix")), "cached title missing: {rows:?}");
        // The song list row is the cached title — the raw URL only appears
        // in the info box's File line, which is correct.
        assert!(
            rows.iter().any(|r| r.contains("S Some Mix") && !r.contains("rr4")),
            "raw stream URL leaked into the song row: {rows:?}"
        );
        // The stream row renders its title in dark blue (the local file
        // row keeps the white list style).
        let blue_cells: Vec<(u16, u16)> = (0..buffer.area.width)
            .flat_map(|x| (0..buffer.area.height).map(move |y| (x, y)))
            .filter(|&(x, y)| buffer[(x, y)].style().fg == Some(Color::Blue))
            .collect();
        assert!(!blue_cells.is_empty(), "no dark-blue stream text found");
        let mix_row = rows.iter().position(|r| r.contains("Some Mix")).expect("stream title row");
        assert!(
            blue_cells.iter().any(|&(_, y)| y as usize == mix_row),
            "the dark-blue cells must sit on the stream title row"
        );
    }
}

/// The Playlists tab's info box for a stream entry shares the video-style
/// layout (title, channel/subs, "Description ↴" + wrapped body) instead of
/// the generic song preview.
mod stream_info_box {
    use super::*;
    use crate::{
        MpdQueryResult,
        shared::ytdlp::YtStreamInfo,
        ui::panes::playlists::FETCH_DATA,
    };
    use ratatui::prelude::Rect;

    #[test]
    fn stream_entry_shows_the_video_style_info() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
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
        let mut pane = PlaylistsPane::new(&ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::playlist_name_only("mix".to_owned())],
                path: None,
            },
            true,
            &ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane.stack_mut().enter();
        pane.on_query_finished(
            FETCH_DATA,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::Song(song("https://rr4.example/audio.m4a"))],
                path: Some("mix".into()),
            },
            true,
            &ctx,
        )
        .unwrap();
        // Select the stream song so the info box shows it.
        pane.stack.current_mut().select_idx(0, 0);

        let backend = ratatui::backend::TestBackend::new(60, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 60, 24), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..24u16)
            .map(|y| {
                (0..60u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Some Mix"), "cached title in the info box: {text}");
        assert!(text.contains("Some Channel"), "channel row in the info box: {text}");
        assert!(text.contains("Description"), "video-style description label: {text}");
        assert!(text.contains("wraps around"), "wrapped description body: {text}");
        // Not the generic preview groups.
        assert!(!text.contains("--- [YouTube]"), "no preview group header: {text}");
        assert!(!text.contains("Last Modified"), "no preview metadata rows: {text}");
    }
}

/// The Playlists tab's info box scrolls: a long stream description
/// overflows into the themed scrollbar, the wheel scrolls it and clicking
/// the scrollbar jumps proportionally — like the other tabs' info boxes.
mod info_scroll {
    use super::*;
    use crate::{
        MpdQueryResult,
        shared::{
            mouse_event::{MouseEvent, MouseEventKind},
            ytdlp::YtStreamInfo,
        },
        ui::panes::playlists::FETCH_DATA,
    };
    use crossterm::event::KeyModifiers;
    use ratatui::prelude::Rect;

    fn long_stream_ctx() -> Ctx {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let description = (0..60)
            .map(|i| format!("paragraph line {i} with words to wrap"))
            .collect::<Vec<_>>()
            .join(" ");
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Some Mix".to_owned(),
                description: Some(description),
                ..Default::default()
            },
        );
        ctx
    }

    fn pane_with_stream(ctx: &Ctx) -> PlaylistsPane {
        let mut pane = PlaylistsPane::new(ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::playlist_name_only("mix".to_owned())],
                path: None,
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane.stack_mut().enter();
        pane.on_query_finished(
            FETCH_DATA,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::Song(song("https://rr4.example/audio.m4a"))],
                path: Some("mix".into()),
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane
    }

    fn render_pane(pane: &mut PlaylistsPane, ctx: &Ctx) {
        // 160 cols: wide enough that the left playlists pane renders (a
        // narrow TUI hides it entirely, so area snapshots would be empty).
        let backend = ratatui::backend::TestBackend::new(160, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 24), ctx).unwrap())
            .unwrap();
    }

    #[test]
    fn long_description_overflows_into_the_themed_scrollbar() {
        let ctx = long_stream_ctx();
        let mut pane = pane_with_stream(&ctx);
        render_pane(&mut pane, &ctx);
        assert!(
            pane.info_items_len > pane.info_area.height as usize,
            "the description must overflow the info box"
        );
        assert!(
            pane.info_scrollbar_area.height > 0,
            "a scrollbar must appear when the info overflows"
        );
    }

    #[test]
    fn wheel_and_scrollbar_click_scroll_the_info_box() {
        let ctx = long_stream_ctx();
        let mut pane = pane_with_stream(&ctx);
        render_pane(&mut pane, &ctx);
        let max = pane.info_items_len.saturating_sub(pane.info_area.height as usize);
        assert!(max > 0);

        // The wheel over the info area scrolls down one row.
        let info = pane.info_area;
        pane.handle_mouse_event(
            MouseEvent {
                x: info.x + 1,
                y: info.y + 1,
                kind: MouseEventKind::ScrollDown,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.info_state.offset(), 1, "wheel down scrolls the info box");

        // Clicking at the bottom of the scrollbar jumps to the end: track
        // clicks put the thumb's top under the pointer, and the bottom row
        // of the bar is past the thumb's travel, so the offset reaches max
        // (the same thumb-follow contract as the other tabs' scrollbars).
        let sb = pane.info_scrollbar_area;
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
        assert_eq!(pane.info_state.offset(), max, "scrollbar click jumps to the position");
        assert!(pane.info_state.offset() > 0, "near the end of the list");

        // The wheel back up returns toward the top.
        pane.handle_mouse_event(
            MouseEvent {
                x: info.x + 1,
                y: info.y + 1,
                kind: MouseEventKind::ScrollUp,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(
            pane.info_state.offset(),
            max.saturating_sub(1),
            "wheel up scrolls back from the jumped position"
        );
    }
}

/// Multi-selection in the songs pane (the playlists tab's right pane):
/// ctrl+click toggles a mark, alt+click range-marks from the anchor, a
/// plain click on another row clears the marks, and Shift+Up/Down
/// (SelectUp/SelectDown) range-select — the queue audio list / MPD right
/// pane behavior.
mod multi_select {
    use std::sync::Arc;

    use ratatui::prelude::Rect;

    use super::*;
    use crate::{
        config::keys::{CommonAction, GlobalAction},
        shared::{
            keys::{ActionEvent, Actions},
            mouse_event::{MouseEvent, MouseEventKind},
        },
        ui::panes::playlists::FETCH_DATA,
        MpdQueryResult,
    };

    /// A pane inside the playlist `mix` with songs s1..s4.
    fn pane_in_playlist(ctx: &Ctx) -> PlaylistsPane {
        let mut pane = PlaylistsPane::new(ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::playlist_name_only("mix".to_owned())],
                path: None,
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane.stack_mut().enter();
        pane.on_query_finished(
            FETCH_DATA,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::Song(song("s1")),
                    DirOrSong::Song(song("s2")),
                    DirOrSong::Song(song("s3")),
                    DirOrSong::Song(song("s4")),
                ],
                path: Some("mix".into()),
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane
    }

    fn render_pane(pane: &mut PlaylistsPane, ctx: &Ctx) {
        // 160 cols so the left playlists pane is visible (it is hidden
        // entirely on TUIs ≤ 120 cols wide) and its areas are real rects.
        let backend = ratatui::backend::TestBackend::new(160, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 24), ctx).unwrap())
            .unwrap();
    }

    fn click(pane: &mut PlaylistsPane, row: u16, modifiers: crossterm::event::KeyModifiers, ctx: &mut Ctx) {
        let area = pane.songs_area;
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x + 1,
                y: area.y + row,
                kind: MouseEventKind::LeftClick,
                modifiers,
            },
            ctx,
        )
        .unwrap();
    }

    fn act(pane: &mut PlaylistsPane, ctx: &mut Ctx, actions: Vec<Actions>) {
        let mut event = ActionEvent::from(Arc::new(actions));
        pane.handle_action(&mut event, ctx).unwrap();
    }

    fn marks(pane: &PlaylistsPane) -> Vec<usize> {
        pane.stack.current().marked().iter().copied().collect()
    }

    fn make_ctx() -> Ctx {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let (client_tx, _client_rx) = crossbeam::channel::unbounded();
        ctx(
            (app_tx, _app_rx),
            (work_tx, _work_rx),
            (client_tx, _client_rx),
        )
    }

    #[test]
    fn shift_up_down_range_selects_in_the_songs_pane() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        // Cursor on s1 (index 0). Shift+Down moves and marks [0..1].
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectDown)]);
        assert_eq!(pane.stack.current().state.get_selected(), Some(1));
        assert_eq!(marks(&pane), vec![0, 1]);
        // Shift+Down again extends to [0..2].
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectDown)]);
        assert_eq!(marks(&pane), vec![0, 1, 2]);
        // Shift+Up backs up and replaces the range: [0..1], row 2 unmarked.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectUp)]);
        assert_eq!(pane.stack.current().state.get_selected(), Some(1));
        assert_eq!(marks(&pane), vec![0, 1]);
    }

    #[test]
    fn shift_range_reanchors_after_esc_clears_the_selection() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);

        // Shift+Down three times marks [0..=3] (anchor 0).
        for _ in 0..3 {
            act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectDown)]);
        }
        assert_eq!(pane.stack.current().state.get_selected(), Some(3));
        assert_eq!(marks(&pane), vec![0, 1, 2, 3]);

        // Esc clears the marks — and (with the fix) the anchor too.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Close)]);
        assert!(marks(&pane).is_empty());

        // Move the cursor back up to row 2, then Shift+Down: the range
        // must start from the new cursor (2), not reach back to the old
        // anchor (0).
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Up)]);
        assert_eq!(pane.stack.current().state.get_selected(), Some(2));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectDown)]);
        assert_eq!(pane.stack.current().state.get_selected(), Some(3));
        assert_eq!(marks(&pane), vec![2, 3]);
    }

    #[test]
    fn esc_with_a_selection_consumes_the_keypress() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectDown)]);
        assert_eq!(marks(&pane), vec![0, 1]);

        // Esc carries both Close (menu) and ShowSettings; clearing the
        // selection must consume the keypress so settings does not open
        // on the same press.
        let mut ev = ActionEvent::from(Arc::new(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(marks(&pane).is_empty(), "Esc clears the selection");
        assert!(ev.is_consumed(), "clearing the selection consumes the keypress");
    }

    #[test]
    fn esc_without_a_selection_leaves_settings_enabled() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        let mut ev = ActionEvent::from(Arc::new(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(marks(&pane).is_empty());
        assert!(!ev.is_consumed(), "no selection: Esc still opens settings");
    }

    #[test]
    fn focused_pane_selection_uses_the_hover_highlight() {
        let ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        let hovered = ctx.config.theme.hovered_item_style.bg;
        let current = ctx.config.theme.current_item_style.bg;

        // Render once so the pane areas are known, then snapshot them.
        render_pane(&mut pane, &ctx);
        let (left, right) = (pane.playlists_area, pane.songs_area);

        let selected_bg = |pane: &mut PlaylistsPane, area: Rect| -> Option<ratatui::style::Color> {
            let backend = ratatui::backend::TestBackend::new(160, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 24), &ctx).unwrap())
                .unwrap();
            terminal.backend().buffer()[(area.x + 1, area.y)].style().bg
        };

        // Inside a playlist the keyboard cursor is on the songs pane
        // (right): it uses the hover highlight, the playlists pane (left)
        // keeps the plain selection.
        assert_eq!(
            selected_bg(&mut pane, left),
            current,
            "the playlists pane keeps the plain selection inside a playlist"
        );
        assert_eq!(
            selected_bg(&mut pane, right),
            hovered,
            "the songs pane holds the keyboard cursor and uses the hover highlight"
        );
    }

    #[test]
    fn at_the_root_the_playlists_list_uses_the_hover_highlight() {
        let ctx = make_ctx();
        let mut pane = PlaylistsPane::new(&ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![DirOrSong::playlist_name_only("mix".to_owned())],
                path: None,
            },
            true,
            &ctx,
        )
        .unwrap();
        pane.stack.root_mut().select_idx(0, 0);
        let hovered = ctx.config.theme.hovered_item_style.bg;
        let current = ctx.config.theme.current_item_style.bg;

        render_pane(&mut pane, &ctx);
        let (left, right) = (pane.playlists_area, pane.songs_area);

        let selected_bg = |pane: &mut PlaylistsPane, area: Rect| -> Option<ratatui::style::Color> {
            let backend = ratatui::backend::TestBackend::new(160, 24);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| pane.render(frame, Rect::new(0, 0, 160, 24), &ctx).unwrap())
                .unwrap();
            terminal.backend().buffer()[(area.x + 1, area.y)].style().bg
        };

        // At the root the keyboard cursor is on the playlists list: the
        // left pane shows the hover highlight; the right pane is a plain
        // mirror of the same list.
        assert_eq!(
            selected_bg(&mut pane, left),
            hovered,
            "the playlists list holds the cursor at the root"
        );
        assert_eq!(
            selected_bg(&mut pane, right),
            current,
            "the right mirror keeps the plain selection"
        );
    }

    #[test]
    fn ctrl_click_toggles_and_plain_click_clears_the_marks() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        render_pane(&mut pane, &ctx);
        // ctrl+click toggles marks on rows 0 and 2.
        click(&mut pane, 0, crossterm::event::KeyModifiers::CONTROL, &mut ctx);
        click(&mut pane, 2, crossterm::event::KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 2]);
        // ctrl+click on an already-marked row unmarks it.
        click(&mut pane, 0, crossterm::event::KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(marks(&pane), vec![2]);
        // A plain click on a different row clears the whole selection.
        click(&mut pane, 3, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert!(marks(&pane).is_empty(), "plain click clears the marks");
        assert_eq!(pane.stack.current().state.get_selected(), Some(3));
    }

    #[test]
    fn alt_click_ranges_from_the_anchor() {
        let mut ctx = make_ctx();
        let mut pane = pane_in_playlist(&ctx);
        render_pane(&mut pane, &ctx);
        // A plain click sets the anchor (row 0).
        click(&mut pane, 0, crossterm::event::KeyModifiers::NONE, &mut ctx);
        // alt+click on row 3 range-marks [0..3].
        click(&mut pane, 3, crossterm::event::KeyModifiers::ALT, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 1, 2, 3]);
        // alt+clicking closer to the anchor replaces the range.
        click(&mut pane, 1, crossterm::event::KeyModifiers::ALT, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 1]);
    }
}

/// The left playlists pane mirrors the MPD folder tree's width behavior:
/// hidden entirely on TUIs ≤ 120 columns wide (the songs pane gets the
/// whole area and scroll lands on it), and a 50-column minimum on wider
/// TUIs.
mod left_pane_width_regimes {
    use ratatui::prelude::Rect;

    use super::*;
    use crate::{
        MpdQueryResult,
        shared::mouse_event::{MouseEvent, MouseEventKind},
    };
    use crossterm::event::KeyModifiers;

    fn ctx() -> Ctx {
        // `fixtures::ctx` only keeps the senders of the work/client
        // channels; forgetting the receivers keeps them open so sends
        // from render/scroll handlers never hit a disconnected channel
        // (same pattern as the round-7 directories tests).
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        std::mem::forget(app_rx.clone());
        std::mem::forget(work_rx.clone());
        std::mem::forget(client_rx.clone());
        crate::tests::fixtures::ctx((app_tx, app_rx), (work_tx, work_rx), (client_tx, client_rx))
    }

    fn pane_with_playlist(ctx: &Ctx) -> PlaylistsPane {
        let mut pane = PlaylistsPane::new(ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::playlist_name_only("mix".to_owned()),
                    DirOrSong::playlist_name_only("films".to_owned()),
                ],
                path: None,
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane
    }

    fn render_at(pane: &mut PlaylistsPane, ctx: &Ctx, width: u16) {
        let backend = ratatui::backend::TestBackend::new(width, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, width, 24), ctx).unwrap())
            .unwrap();
    }

    #[test]
    fn left_pane_hidden_on_narrow_tui() {
        let ctx = ctx();
        let mut pane = PlaylistsPane::new(&ctx);
        // Enough playlists that the right pane's list overflows the
        // viewport, so a scroll visibly moves its offset.
        let playlists: Vec<DirOrSong> = (0..12)
            .map(|i| DirOrSong::playlist_name_only(format!("pl{i}")))
            .collect();
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong { data: playlists, path: None },
            true,
            &ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        render_at(&mut pane, &ctx, 80);
        // The playlists pane is not rendered at all: its rect stays
        // default, so mouse events (scroll included) over the left
        // columns fall through to the songs pane.
        assert_eq!(pane.playlists_area, Rect::default(), "left pane not rendered on a narrow TUI");
        assert_eq!(pane.songs_area.x, 1, "songs pane starts at the left edge");
        assert_eq!(pane.songs_area.width, 78, "songs pane takes the whole width");

        // Scrolling over the leftmost column drives the songs pane.
        pane.handle_mouse_event(
            MouseEvent {
                x: 1,
                y: 5,
                kind: MouseEventKind::ScrollDown,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(
            pane.stack.current().state.offset(),
            1,
            "scroll on a narrow TUI moves the right pane's list"
        );
    }

    #[test]
    fn left_pane_keeps_min_width_on_wide_tui() {
        let ctx = ctx();
        let mut pane = pane_with_playlist(&ctx);
        render_at(&mut pane, &ctx, 160);
        // 50-col tree pane minus its 2 border columns.
        assert_eq!(pane.playlists_area.width, 48, "left pane keeps its 50-col minimum");
        assert!(
            pane.songs_area.x >= pane.playlists_area.width,
            "songs pane starts after the left pane"
        );
    }
}

/// Round 9: the info box's height is capped at 15 rows on tall terminals
/// (it would otherwise be (h−3)×2/3 — 38 rows at h=60 — stealing the songs
/// list's space). Panes ≤ ~25 rows tall are unchanged: there the 2/3 split
/// is already ≤ 15, so the info box keeps its exact length and the songs
/// list fills the rest.
mod info_box_height_cap {
    use ratatui::prelude::Rect;

    use super::*;
    use crate::MpdQueryResult;

    fn ctx() -> Ctx {
        // Same receiver-keeping pattern as the width-regime tests.
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        std::mem::forget(app_rx.clone());
        std::mem::forget(work_rx.clone());
        std::mem::forget(client_rx.clone());
        crate::tests::fixtures::ctx((app_tx, app_rx), (work_tx, work_rx), (client_tx, client_rx))
    }

    fn pane_with_playlist(ctx: &Ctx) -> PlaylistsPane {
        let mut pane = PlaylistsPane::new(ctx);
        pane.on_query_finished(
            super::super::INIT,
            MpdQueryResult::DirOrSong {
                data: vec![
                    DirOrSong::playlist_name_only("mix".to_owned()),
                    DirOrSong::playlist_name_only("films".to_owned()),
                ],
                path: None,
            },
            true,
            ctx,
        )
        .unwrap();
        pane.stack.current_mut().select_idx(0, 0);
        pane
    }

    fn render_at(pane: &mut PlaylistsPane, ctx: &Ctx, width: u16, height: u16) {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, width, height), ctx).unwrap())
            .unwrap();
    }

    #[test]
    fn info_box_capped_at_15_on_a_tall_render() {
        let ctx = ctx();
        let mut pane = pane_with_playlist(&ctx);
        // 40 rows: the uncapped 2/3 split would give the info box 24
        // rows — the cap keeps it at 15 and the songs list takes the rest.
        render_at(&mut pane, &ctx, 160, 40);
        assert_eq!(pane.info_area.height, 15, "info box capped at 15 rows");
        // 40 − 3 tips − 15 info − 2 borders = the songs list gets 20 rows.
        assert_eq!(pane.songs_area.height, 20, "songs list takes the remainder");
    }

    #[test]
    fn short_panes_keep_the_uncapped_two_thirds_split() {
        let ctx = ctx();
        let mut pane = pane_with_playlist(&ctx);
        // 20 rows: (20−3)×2/3 = 11 ≤ 15, so the cap does not engage and
        // the info box keeps its exact 2/3 length (round-8 behavior).
        render_at(&mut pane, &ctx, 160, 20);
        assert_eq!(pane.info_area.height, 11, "uncapped 2/3 split below 25 rows");
        // 20 − 3 tips − 11 info − 2 borders = the songs list gets 4 rows.
        assert_eq!(pane.songs_area.height, 4, "songs list fills the remainder");
    }
}
