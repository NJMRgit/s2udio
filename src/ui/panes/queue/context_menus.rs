// Context menus of the Queue tab: the audio queue menu and the video
// queue menu (plus the dispatcher between them). Split out of the queue
// module root (queue.rs) so the pane keeps its inherent-method surface
// while each focus area lives in its own file.
use itertools::Itertools;

use super::{play_queue_song, QueuePane};
use crate::{
    ctx::Ctx,
    mpd::mpd_client::MpdClient,
    shared::{
        ext::btreeset_ranges::BTreeSetRanges,
        macros::{modal, status_warn},
        mpd_client_ext::MpdClientExt,
    },
    ui::{
        UiAppEvent,
        modals::{
            confirm_modal::ConfirmModal,
            info_list_modal::InfoListModal,
            input_modal::InputModal,
            menu::modal::MenuModal,
            select_modal::SelectModal,
        },
    },
};

impl QueuePane {
    pub(super) fn open_context_menu(&mut self, ctx: &Ctx) {
        // The menu acts on the list currently shown: in Video mode it
        // manages the video queue (never the MPD audio queue), and in Audio
        // mode it manages the MPD queue.
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video {
            self.open_video_context_menu(ctx);
            return;
        }
        self.open_audio_context_menu(ctx);
    }

    /// Video-mode context menu: play / remove / clear the **video queue**
    /// (the persistent playlist). While a Jellyfin item plays, the list is
    /// the live mpv session's own playlist — Remove and Clear are hidden
    /// (live mpv state).
    pub(super) fn open_video_context_menu(&mut self, ctx: &Ctx) {
        let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
        let selected_idx = self.video_state.selected();
        let playlist: Vec<crate::core::mpv::MpvPlaylistEntry> = if jellyfin {
            ctx.mpv.playlist.borrow().clone()
        } else {
            ctx.video_playlist.borrow().clone()
        };
        let title = selected_idx
            .and_then(|i| playlist.get(i))
            .map(|e| e.title.clone())
            .unwrap_or_else(|| " Video ".to_owned());
        // A stream entry (resolved YouTube/Soundcloud link): offer Download,
        // which saves it into s2udio-downloads and replaces the entry with
        // the file.
        let download_stream = selected_idx
            .and_then(|i| playlist.get(i))
            .and_then(|entry| {
                let info = ctx.yt_info.borrow();
                info.get(&entry.url).cloned().or_else(|| {
                    info.values().find(|e| e.original_url == entry.url).cloned()
                })
            })
            .map(|info| (info, selected_idx.unwrap_or(0)));

        let menu = MenuModal::new(ctx)
            .width(60)
            .title(format!(" {title} "))
            .list_section(ctx, |section| {
                let mut section = section;
                if let Some(play_idx) = selected_idx {
                    section = section.item("Play from here", move |ctx| {
                        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> = if jellyfin {
                            ctx.mpv.playlist.borrow().iter().skip(play_idx).cloned().collect()
                        } else {
                            ctx.video_playlist.borrow().iter().skip(play_idx).cloned().collect()
                        };
                        if !entries.is_empty() {
                            crate::core::mpv::play_video_entries(ctx, entries);
                        }
                        Ok(())
                    });
                }
                if let Some((info, index)) = download_stream {
                    section = section.item("Download", move |ctx| {
                        crate::ui::modals::paste::open_stream_download_menu(
                            ctx,
                            &info,
                            &crate::shared::ytdlp::ReplaceAction::VideoPlaylist { index },
                        );
                        Ok(())
                    });
                }
                if !jellyfin {
                    // Remove deletes every marked entry (like the audio
                    // queue list), or just the highlighted one.
                    let marked_indices: Vec<usize> = self.video_marked.iter().collect();
                    let remove_uses_marks = !marked_indices.is_empty();
                    section = section.item("Remove", move |ctx| {
                        let remove: Vec<usize> = if !marked_indices.is_empty() {
                            marked_indices.clone()
                        } else {
                            selected_idx.into_iter().collect()
                        };
                        {
                            let mut playlist = ctx.video_playlist.borrow_mut();
                            for idx in remove.iter().rev() {
                                if *idx < playlist.len() {
                                    playlist.remove(*idx);
                                }
                            }
                        }
                        crate::ui::modals::paste::save_video_playlist(ctx);
                        if remove_uses_marks {
                            // The marked indices no longer exist after the
                            // removals; drop the selection.
                            ctx.app_event_sender.send(crate::AppEvent::UiEvent(
                                UiAppEvent::ClearQueueMarked,
                            ))?;
                        }
                        ctx.render()?;
                        Ok(())
                    });
                    section = section.item("Clear video queue", move |ctx| {
                        modal!(
                            ctx,
                            ConfirmModal::builder()
                                .ctx(ctx)
                                .message(vec![
                                    "Are you sure you want to clear the video queue?",
                                    "This cannot be undone (playing video keeps playing).",
                                ])
                                .action(crate::ui::modals::confirm_modal::Action::Single {
                                    on_confirm: Box::new(|ctx| {
                                        ctx.video_playlist.borrow_mut().clear();
                                        crate::ui::modals::paste::save_video_playlist(ctx);
                                        ctx.render()?;
                                        Ok(())
                                    }),
                                    confirm_label: Some("Clear"),
                                    cancel_label: None,
                                })
                                .size((45, 6))
                                .build()
                        );
                        Ok(())
                    });
                    // The persistent video queue becomes a stored MPD
                    // playlist (video-only by construction). Jellyfin
                    // sessions are not eligible: their list is the live mpv
                    // playlist, and Jellyfin video already has its own
                    // queue handling.
                    section = section.item("Create video playlist", move |ctx| {
                        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> =
                            ctx.video_playlist.borrow().clone();
                        if entries.is_empty() {
                            status_warn!("The video queue is empty");
                            return Ok(());
                        }
                        modal!(
                            ctx,
                            InputModal::new(ctx)
                                .title("Create video playlist")
                                .confirm_label("Save")
                                .input_label("Playlist name:")
                                .on_confirm(move |ctx, value| {
                                    let value = value.to_owned();
                                    let uris: Vec<String> =
                                        entries.iter().map(|e| e.url.clone()).collect();
                                    ctx.command(move |client| {
                                        client.create_playlist(&value, uris)?;
                                        Ok(())
                                    });
                                    Ok(())
                                })
                        );
                        Ok(())
                    });
                }
                Some(section)
            })
            .list_section(ctx, |section| Some(section.item("Cancel", |_ctx| Ok(()))))
            .build();

        modal!(ctx, menu);
    }

    pub(super) fn open_audio_context_menu(&mut self, ctx: &Ctx) {
        let selected_song = self.queue.selected().cloned();
        let selected_song_id = selected_song.as_ref().map(|s| s.id);
        // A resolved YouTube-style stream row (the queue entry holds the
        // resolved stream URL, or the original link): offer Download, which
        // saves it into s2udio-downloads and replaces the row with the file.
        let download_ctx = selected_song.as_ref().and_then(|song| {
            let info = ctx.yt_info.borrow();
            info.get(&song.file).cloned().or_else(|| {
                info.values().find(|e| e.original_url == song.file).cloned()
            })
        }).map(|info| (info, selected_song_id.unwrap_or(u32::MAX)));
        // Marked ranges are deleted together when the menu's Remove is
        // picked (highest range first, so the indices stay valid).
        let marked_ranges: Vec<std::ops::RangeInclusive<usize>> =
            self.queue.marked().ranges().collect();

        let modal = MenuModal::new(ctx)
            .list_section(ctx, |mut section| {
                let play_song = selected_song.clone();
                section.add_item("Play", move |ctx| {
                    if let Some(song) = play_song.as_ref() {
                        play_queue_song(song, ctx);
                    }
                    Ok(())
                });
                section.add_item("Show info", move |ctx| {
                    if let Some(song) = selected_song {
                        modal!(
                            ctx,
                            InfoListModal::builder()
                                .rows(&song)
                                .title("Song info")
                                .column_widths(&[30, 70])
                                .build()
                        );
                    }
                    Ok(())
                });
                if let Some((info, song_id)) = download_ctx {
                    section.add_item("Download", move |ctx| {
                        crate::ui::modals::paste::open_stream_download_menu(
                            ctx,
                            &info,
                            &crate::shared::ytdlp::ReplaceAction::Queue { song_id },
                        );
                        Ok(())
                    });
                }
                Some(section)
            })
            .list_section(ctx, |mut section| {
                // The visible queue rows only: hidden entries (the temporary
                // "play without adding to queue" item, radio/stream rows)
                // never leak into a playlist.
                let items = self.queue.items.iter().map(|song| song.file.clone()).collect_vec();
                let items_add = items.clone();
                section.add_item("Add queue to playlist", move |ctx| {
                    // The radio favourites playlist is Radio-tab-owned: it
                    // never appears as an add target.
                    let radio_playlist = ctx.config.radio.playlist.clone();
                    let playlists = ctx.query_sync(move |client| {
                        Ok(client
                            .picker_playlists(&radio_playlist)?
                            .into_iter()
                            .map(|p| p.name)
                            .collect_vec())
                    })?;
                    let items = items_add.clone();

                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                let items = items.clone();
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(&selected, items)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                section.add_item("Create audio playlist", move |ctx| {
                    if items.is_empty() {
                        status_warn!("No songs in the queue to save");
                        return Ok(());
                    }
                    let items = items.clone();
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create audio playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                let items = items.clone();
                                ctx.command(move |client| {
                                    client.create_playlist(&value, items)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });

                Some(section)
            })
            .list_section(ctx, |section| {
                let section = section
                    .item("Remove", move |ctx| {
                        if !marked_ranges.is_empty() {
                            for range in marked_ranges.iter().rev() {
                                let range = range.clone();
                                ctx.command(move |client| {
                                    client.delete_from_queue(range.into())?;
                                    Ok(())
                                });
                            }
                            // The marked indices no longer exist after the
                            // deletions; drop the selection.
                            ctx.app_event_sender
                                .send(crate::AppEvent::UiEvent(UiAppEvent::ClearQueueMarked))?;
                        } else if let Some(id) = selected_song_id {
                            ctx.command(move |client| {
                                client.delete_id(id)?;
                                Ok(())
                            });
                        }
                        Ok(())
                    })
                    .item("Clear queue", |ctx| {
                        ctx.command(|client| {
                            client.clear()?;
                            Ok(())
                        });
                        Ok(())
                    });
                Some(section)
            })
            .list_section(ctx, |section| {
                let section = section.item("Cancel", |_ctx| Ok(()));
                Some(section)
            })
            .build();

        modal!(ctx, modal);
    }
}
