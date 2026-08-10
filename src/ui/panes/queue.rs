use std::collections::HashSet;

use anyhow::Result;
use enum_map::{Enum, EnumMap, enum_map};
use itertools::Itertools;
use ratatui::{
    Frame,
    layout::Flex,
    prelude::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Row, StatefulWidget, TableState},
};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction,
            DirectoriesActions,
            GlobalAction,
            QueueActions,
            actions::{AddKind, AutoplayKind},
        },
        theme::{
            AlbumSeparator,
            properties::{Property, SongProperty},
        },
    },
    core::command::{create_env, run_external},
    ctx::Ctx,
    mpd::{
        QueuePosition,
        client::Client,
        commands::{Song, State},
        mpd_client::{MpdClient, ValueChange},
    },
    shared::{
        ext::{btreeset_ranges::BTreeSetRanges, rect::RectExt},
        keys::ActionEvent,
        macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
        song_ext::SongsExt,
    },
    ui::{
        UiAppEvent,
        UiEvent,
        dirstack::{Dir, MarkState},
        song_list::SongListCore,
        input::InputResultEvent,
        modals::{
            confirm_modal::{Action, ConfirmModal},
            info_list_modal::InfoListModal,
            input_modal::InputModal,
            menu::{
                create_add_modal,
                modal::MenuModal,
            },
            select_modal::SelectModal,
        },
        panes::queue_header::QueueHeaderPane,
        widgets::virtualized_table::VirtualizedTable,
    },
};

#[derive(Debug)]
pub struct QueuePane {
    queue: Dir<Song, TableState>,
    column_widths: Vec<Constraint>,
    column_formats: Vec<Property<SongProperty>>,
    areas: EnumMap<Areas, Rect>,
    should_center_cursor_on_current: bool,
    /// Scroll state of the chapter list (Chapters mode).
    chapters_state: ListState,
    chapters_items_len: usize,
    /// Scroll state of the mpv playlist (Video mode).
    video_state: ListState,
    video_items_len: usize,
    /// Marked (multi-selected) entries of the mpv playlist (Video mode),
    /// with the ctrl/alt-click + shift+up/down selection of the audio
    /// queue list.
    video_marked: MarkState,
    /// Drag state of the Video-list scrollbar (thumb follows the pointer).
    video_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Drag state of the Chapters-list scrollbar (thumb follows the
    /// pointer).
    chapters_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag,
    /// Click areas of the Audio / Video / Chapters toggle.
    pub(crate) toggle_areas: [Rect; 3],
}

#[derive(Debug, Enum)]
enum Areas {
    Table,
    Scrollbar,
    FilterArea,
}

const ADD_TO_PLAYLIST: &str = "add_to_playlist";
const ADD_TO_PLAYLIST_MULTIPLE: &str = "add_to_playlist_multiple";
/// Result id of a local file's chapter markers (from ffprobe).
pub const FILE_CHAPTERS: &str = "file_chapters";

/// Width (in cells) of the Time / Duration columns in the chapters table.
/// The chapters table uses its own columns — Chapter (flexible) | Time
/// (centered) | Duration (right-aligned at the right edge, matching the
/// queue list's Duration column) — shared with the QueueHeaderPane's
/// chapters header so the labels and values line up.
pub(crate) const CHAPTER_TIME_COL: u16 = 10;
pub(crate) const CHAPTER_DURATION_COL: u16 = 10;

/// Whether `file` is a resolved YouTube stream whose signed URL has
/// expired. googlevideo `videoplayback` URLs carry an `expire` epoch; once
/// it passes, MPD cannot open the stream (the YouTube video itself may
/// still exist — only the signed link died).
fn resolved_stream_expired(file: &str) -> bool {
    if !file.contains("googlevideo.com") || !file.contains("videoplayback") {
        return false;
    }
    let Some(expire) = file
        .split("expire=")
        .nth(1)
        .and_then(|rest| rest.split('&').next())
        .and_then(|value| value.parse::<i64>().ok())
    else {
        return false;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    expire < now
}

/// Play a queue song. A resolved YouTube stream whose signed URL expired
/// cannot be played as-is: when the cached info still knows its original
/// link, the link is re-resolved and the dead entry replaced in place
/// ([`YtAction::ReplaceAndPlay`]); otherwise the failure is explained
/// instead of failing silently in MPD.
fn play_queue_song(song: &crate::mpd::commands::Song, ctx: &Ctx) {
    if resolved_stream_expired(&song.file) {
        let original = ctx.yt_info.borrow().get(&song.file).and_then(|yt| {
            let url = yt.original_url.clone();
            (!url.is_empty() && url != song.file).then_some(url)
        });
        if let Some(original) = original {
            let id = song.id;
            let _ = ctx
                .work_sender
                .send(crate::shared::events::WorkRequest::ResolveYtStreams {
                    urls: vec![original],
                    action: crate::ui::modals::paste::YtAction::ReplaceAndPlay(id),
                })
                .map_err(|err| log::error!(error:? = err; "Failed to request stream re-resolution"));
            status_info!("Stream URL expired — re-resolving from the original link");
            return;
        }
        status_warn!("This queue entry's stream URL expired; add the original link again");
    }
    let id = song.id;
    ctx.command(move |client| {
        client.play_id(id)?;
        Ok(())
    });
}

impl QueuePane {
    /// Drop the multi-selected (marked) set, e.g. after the context-menu
    /// Remove deleted the items.
    pub(crate) fn clear_marked(&mut self) {
        self.queue.marked_mut().clear();
        self.queue.state.clear_mark_anchor();
        self.video_marked.clear();
        self.video_marked.clear_anchor();
    }

    /// The queue the pane shows. Radio stations and Jellyfin audio streams
    /// are played through a temporary MPD playlist entry (required to play
    /// a stream) and filtered out here so they never show up in the Queue
    /// tab; the same applies to files played from Directories with the
    /// right arrow / double-click and the paste popup's "Play (don't add
    /// to queue)" — their temporary entry is hidden until it is dropped.
    /// Resolved YouTube-style streams are **queue content** (added via the
    /// paste popup's Add/Append): they stay visible, keyed by their URL in
    /// the yt-info cache.
    fn local_queue(ctx: &Ctx) -> Vec<Song> {
        let temp_play = ctx.temp_play_id.get();
        ctx.queue
            .iter()
            .filter(|song| {
                let hidden_stream = crate::ui::panes::radio::is_stream_url(&song.file)
                    && !ctx.yt_info.borrow().contains_key(&song.file);
                !hidden_stream && Some(song.id) != temp_play
            })
            .cloned()
            .collect()
    }

    /// Whether the current song has chapter markers (shows the
    /// Audio / Video / Chapters toggle).
    fn chapters_available(ctx: &Ctx) -> bool {
        ctx.has_current_chapters()
    }

    /// Chapter markers of the current playback (mpv video or queue song).
    fn current_chapters(ctx: &Ctx) -> Vec<crate::shared::chapters::Chapter> {
        ctx.current_playback_chapters()
    }

    /// Switch the Queue tab's list to `mode`, resetting the list
    /// highlights and landing the Chapters/Video highlight on the currently
    /// playing item.
    fn set_tab(&mut self, ctx: &Ctx, mode: crate::ctx::QueueTabMode) {
        if ctx.queue_tab.get() == mode {
            return;
        }
        ctx.queue_tab.set(mode);
        self.chapters_state = ListState::default();
        self.video_state = ListState::default();
        // The video marks belong to the list that was just switched away
        // from; a fresh list starts unmarked.
        self.video_marked.clear();
        match mode {
            crate::ctx::QueueTabMode::Chapters => self.chapters_select_current(ctx),
            crate::ctx::QueueTabMode::Video => {
                let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
                let playlist: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> =
                    if jellyfin { ctx.mpv.playlist.borrow() } else { ctx.video_playlist.borrow() };
                let current = if jellyfin {
                    ctx.mpv.playlist_pos.get()
                } else {
                    crate::core::mpv::video_playlist_current_idx(ctx)
                };
                if let Some(idx) = current.filter(|i| *i < playlist.len()) {
                    self.video_state.select(Some(idx));
                } else if !playlist.is_empty() {
                    self.video_state.select(Some(0));
                }
            }
            crate::ctx::QueueTabMode::Audio => {}
        }
    }

    /// Cycle the list view: Audio -> Video -> Chapters -> Audio (Chapters
    /// only when the track has markers).
    fn cycle_tab(&mut self, ctx: &Ctx) {
        let next = match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Audio => crate::ctx::QueueTabMode::Video,
            crate::ctx::QueueTabMode::Video => {
                if Self::chapters_available(ctx) {
                    crate::ctx::QueueTabMode::Chapters
                } else {
                    crate::ctx::QueueTabMode::Audio
                }
            }
            crate::ctx::QueueTabMode::Chapters => crate::ctx::QueueTabMode::Audio,
        };
        Self::set_tab(self, ctx, next);
    }

    /// The list the Queue tab should show while a video plays in mpv: its
    /// Chapters list when the video has markers (and the auto-chapters
    /// setting allows it), else the mpv playlist (Video list). Called when
    /// a video session starts (launch, reattach) and when the video's
    /// chapters arrive, so the tab never keeps showing the stale audio list
    /// after a video was added. A no-op while the video is not the active
    /// UI source (nothing plays in mpv, or MPD playback has taken over and
    /// paused it — the Queue list then belongs to the music).
    pub(crate) fn follow_playing_video(&mut self, ctx: &Ctx) {
        if !crate::core::mpv::mpv_is_ui_source(ctx) {
            return;
        }
        let mode = if ctx.config.ui.auto_show_chapters && Self::chapters_available(ctx) {
            crate::ctx::QueueTabMode::Chapters
        } else {
            crate::ctx::QueueTabMode::Video
        };
        Self::set_tab(self, ctx, mode);
    }

    pub fn new(ctx: &Ctx) -> Self {
        let (column_widths, column_formats) = Self::init(ctx);

        Self {
            queue: Dir::new(Self::local_queue(ctx)),
            column_widths,
            column_formats,
            areas: enum_map! {
                _ => Rect::default(),
            },
            should_center_cursor_on_current: ctx.config.center_current_song_on_change,
            chapters_state: ListState::default(),
            chapters_items_len: 0,
            video_state: ListState::default(),
            video_items_len: 0,
            video_marked: MarkState::default(),
            video_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            chapters_scrollbar_drag: crate::shared::mouse_event::ScrollbarDrag::default(),
            toggle_areas: [Rect::default(); 3],
        }
    }

    pub fn init(ctx: &Ctx) -> (Vec<Constraint>, Vec<Property<SongProperty>>) {
        (
            ctx.config
                .theme
                .song_table_format
                .iter()
                // This 0 is fine - song_table_format should never have the Ratio constraint
                .map(|v| v.width.into_constraint(0))
                .collect_vec(),
            ctx.config.theme.song_table_format.iter().map(|v| v.prop.clone()).collect_vec(),
        )
    }

    fn enqueue_items(&self, all: bool) -> (Vec<Enqueue>, Option<usize>) {
        let hovered = self.queue.selected().map(|s| s.file.as_str());
        self.items(all).fold((Vec::new(), None), |mut acc, (idx, song)| {
            let path = song.file.clone();
            if hovered.as_ref().is_some_and(|hovered| hovered == &path) {
                acc.1 = Some(idx);
            }

            acc.0.push(Enqueue::File { path });

            acc
        })
    }

    fn items<'a>(&'a self, all: bool) -> Box<dyn Iterator<Item = (usize, &'a Song)> + 'a> {
        if all {
            Box::new(self.queue.items.iter().enumerate())
        } else if self.queue.marked().is_empty() {
            if let Some((idx, item)) = self.queue.selected_with_idx() {
                Box::new(std::iter::once((idx, item)))
            } else {
                Box::new(std::iter::empty::<(usize, &Song)>())
            }
        } else {
            Box::new(self.queue.marked().iter().map(|idx| (*idx, &self.queue.items[*idx])))
        }
    }

    fn open_context_menu(&mut self, ctx: &Ctx) {
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
    fn open_video_context_menu(&mut self, ctx: &Ctx) {
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

    fn open_audio_context_menu(&mut self, ctx: &Ctx) {
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
                                .items(&song)
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

impl QueuePane {
    /// The `● Audio ○ Video ○ Chapters` toggle, drawn on the row above the
    /// box that contains the queue list (the config reserves a 1-row spacer
    /// there). TabScreen calls this after the box block renders; the active
    /// tab gets the filled dot and clicking a segment switches the mode (`c`
    /// keybind cycles). The ●/○ glyphs are both single-width so the row
    /// never shifts between modes. Audio and Video always show; Chapters
    /// appears when the current track has markers.
    pub(crate) fn render_toggle_on_border(
        &mut self,
        frame: &mut Frame,
        _pane_borders: Borders,
        block_area: Rect,
        ctx: &Ctx,
    ) {
        self.toggle_areas = [Rect::default(); 3];
        if block_area.height == 0 || block_area.y < 2 {
            return;
        }
        // A Chapters mode with no chaptered track falls back to Audio.
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
            && !Self::chapters_available(ctx)
        {
            ctx.queue_tab.set(crate::ctx::QueueTabMode::Audio);
        }
        // Locate the top border row of the box containing this pane: walk
        // up from the pane to the first corner glyph at its left margin.
        let corner_x = block_area.x.saturating_sub(2);
        let border_y = {
            let buf = frame.buffer_mut();
            (1..block_area.y).rev().find(|&row| {
                is_box_corner_glyph(buf[(corner_x, row)].symbol())
            })
        };
        let Some(border_y) = border_y else { return };
        let y = border_y.saturating_sub(1);

        let base = ctx
            .config
            .theme
            .text_color
            .map_or_else(Style::default, |c| Style::default().fg(c));
        let dim = base.add_modifier(Modifier::DIM);
        let active = ctx.queue_tab.get();
        let chapters_on = active == crate::ctx::QueueTabMode::Chapters;
        let chapters_visible = Self::chapters_available(ctx);

        // One cell right of the box corner (the leading space), matching
        // ` ● Audio ○ Video ○ Chapters` on its own row above the box.
        let mut x = corner_x + 1;
        let right = block_area.right().saturating_sub(1);
        let mouse = ctx.mouse_pos();
        let mut seg = |x: &mut u16,
                       areas: &mut [Rect; 3],
                       idx: usize,
                       label: String,
                       on: bool| {
            let w = (label.width() as u16).min(right.saturating_sub(*x));
            let area = Rect { x: *x, y, width: w, height: 1 };
            let base_style = if on { base.add_modifier(Modifier::BOLD) } else { dim };
            // Hovering a toggle lightens it (clickable text).
            let style =
                if mouse.is_some_and(|p| area.contains(p)) {
                    crate::config::hover_style(base_style)
                } else {
                    base_style
                };
            frame.render_widget(Line::styled(label, style), area);
            areas[idx] = area;
            *x += w;
        };
        seg(
            &mut x,
            &mut self.toggle_areas,
            0,
            if active == crate::ctx::QueueTabMode::Audio {
                " ● Audio ".to_owned()
            } else {
                " ⭘ Audio ".to_owned()
            },
            active == crate::ctx::QueueTabMode::Audio,
        );
        seg(
            &mut x,
            &mut self.toggle_areas,
            1,
            if active == crate::ctx::QueueTabMode::Video {
                " ● Video ".to_owned()
            } else {
                " ⭘ Video ".to_owned()
            },
            active == crate::ctx::QueueTabMode::Video,
        );
        if chapters_visible {
            seg(
                &mut x,
                &mut self.toggle_areas,
                2,
                if chapters_on { " ● Chapters " } else { " ⭘ Chapters " }.to_owned(),
                chapters_on,
            );
        }
    }

    /// The chapter list (Chapter | start | duration), replacing the song
    /// table in Chapters mode. The values are laid out in the same columns as
    /// the QueueHeaderPane's `Chapter | Time | Duration` labels, so they line
    /// up underneath them. A click highlights a chapter; clicking the
    /// highlighted chapter again seeks to it (MPD or mpv).
    fn render_chapters(&mut self, frame: &mut Frame, ctx: &Ctx) -> Result<()> {
        let chapters = Self::current_chapters(ctx);
        let fmt = &ctx.config.duration_format;
        let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.position
        } else {
            ctx.status.elapsed.as_secs_f64()
        };
        let current_idx = chapters
            .iter()
            .rposition(|c| position >= c.start_secs)
            .unwrap_or(0);
        self.chapters_items_len = chapters.len();

        // The chapters table uses its own columns (matching the chapters
        // header): Chapter (flexible) | Time (centered) | Duration
        // (right-aligned at the right edge, like the queue's Duration column).
        let area = self.areas[Areas::Table];
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            area,
            self.chapters_state.offset(),
            chapters.len(),
            1,
        );
        let widths = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(CHAPTER_TIME_COL),
            Constraint::Length(CHAPTER_DURATION_COL),
        ])
        .flex(Flex::Start)
        .spacing(1)
        .split(area);
        // The marker prefix (❯ / two spaces) lives inside the chapter column.
        let title_field = (widths[0].width as usize).saturating_sub(2);
        let time_w = widths[1].width as usize;
        let duration_w = widths[2].width as usize;

        let items: Vec<ListItem> = chapters
            .iter()
            .enumerate()
            .map(|(idx, chapter)| {
                let is_current = idx == current_idx;
                let style = if hover_idx == Some(idx) {
                    ctx.config.theme.hovered_item_style
                } else if is_current {
                    ctx.config.theme.current_item_style
                } else {
                    ctx.config.as_list_text_style()
                };
                let start = fmt.format(chapter.start_secs as u64);
                let duration = fmt.format(chapter.duration() as u64);
                let prefix = if is_current { "❯ " } else { "  " };
                let mut title = chapter.title.clone();
                // Width-safe truncation: keep graphemes until the title
                // column is full.
                truncate_to_width(&mut title, title_field);
                // Pad by display width (not char count), so wide glyphs
                // (CJK etc.) can never push the time/duration columns right.
                let title_pad = title_field.saturating_sub(title.width());
                // Time is centered in its column; the duration is
                // right-aligned at the table's right edge.
                let pad_left = time_w.saturating_sub(start.width()) / 2;
                let pad_right = time_w.saturating_sub(start.width() + pad_left);
                let dur_pad = duration_w.saturating_sub(duration.width());
                ListItem::new(Line::styled(
                    format!(
                        "{prefix}{title}{} {}{start}{} {}{duration}",
                        " ".repeat(title_pad),
                        " ".repeat(pad_left),
                        " ".repeat(pad_right),
                        " ".repeat(dur_pad),
                    ),
                    style,
                ))
            })
            .collect();

        // The click-selected chapter (first click highlights it, the second
        // seeks) gets the accent highlight.
        let list = List::new(items).highlight_style(if hover_idx == self.chapters_state.selected() {
            ctx.config.theme.hovered_item_style
        } else {
            ctx.config.theme.highlighted_item_style
        });
        StatefulWidget::render(list, area, frame.buffer_mut(), &mut self.chapters_state);

        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            let max = self.chapters_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
            let position = self.chapters_state.offset().min(max);
            // content_length = max + 1 so the bottom position is reachable
            // (ratatui clamps positions to content_length - 1); the viewport
            // length keeps the thumb proportional to the visible rows.
            StatefulWidget::render(
                scrollbar,
                self.areas[Areas::Scrollbar],
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max + 1)
                    .position(position)
                    .viewport_content_length(self.areas[Areas::Table].height as usize),
            );
        }
        Ok(())
    }

    /// Seek to a chapter start (MPD or the mpv session): the source whose
    /// chapters the list is showing.
    fn seek_to(&self, seconds: f64, ctx: &Ctx) {
        if crate::core::mpv::mpv_is_ui_source(ctx)
            && let Some(socket) = ctx.mpv.socket.clone()
        {
            crate::core::mpv::mpv_seek(&socket, seconds);
            return;
        }
        ctx.command(move |client| {
            use crate::mpd::mpd_client::ValueChange;
            let _ = client.seek_current(ValueChange::Set(seconds.max(0.0) as u32));
            Ok(())
        });
    }

    /// The list shown in the Video view: the Jellyfin session's own
    /// playlist (the season episodes actually playing) while a Jellyfin
    /// item plays, else the persistent video playlist (which is left
    /// untouched during Jellyfin playback and returns when it stops).
    fn render_video(&mut self, frame: &mut Frame, ctx: &Ctx) -> Result<()> {
        let area = self.areas[Areas::Table];
        let jellyfin = crate::core::mpv::session_playlist_shown(ctx);
        let playlist: std::cell::Ref<'_, Vec<crate::core::mpv::MpvPlaylistEntry>> = if jellyfin {
            ctx.mpv.playlist.borrow()
        } else {
            ctx.video_playlist.borrow()
        };
        self.video_items_len = playlist.len();
        // The playlist can change under the marks (session switches,
        // removals elsewhere); drop any mark that no longer has a row.
        self.video_marked.clamp(playlist.len());
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            area,
            self.video_state.offset(),
            playlist.len(),
            1,
        );
        if let Some(sel) = self.video_state.selected() {
            if playlist.is_empty() {
                self.video_state.select(None);
            } else if sel >= playlist.len() {
                self.video_state.select(Some(playlist.len() - 1));
            }
        }

        if playlist.is_empty() {
            let style = ctx.config.as_list_text_style().add_modifier(Modifier::DIM);
            frame.render_widget(
                ratatui::widgets::Paragraph::new("No video playing").style(style),
                Rect { x: area.x + 1, y: area.y, width: area.width.saturating_sub(2), height: 1 },
            );
            return Ok(());
        }

        let fmt = &ctx.config.duration_format;
        let current_idx = if jellyfin {
            ctx.mpv.playlist_pos.get().filter(|i| *i < playlist.len())
        } else {
            crate::core::mpv::video_playlist_current_idx(ctx).filter(|i| *i < playlist.len())
        };
        // Title (flexible) | Duration (right-aligned at the right edge,
        // like the queue's Duration column).
        let widths = Layout::horizontal([
            Constraint::Min(0),
            Constraint::Length(CHAPTER_DURATION_COL),
        ])
        .flex(Flex::Start)
        .spacing(1)
        .split(area);
        let title_field = widths[0].width as usize;
        let duration_w = widths[1].width as usize;

        let items: Vec<ListItem> = playlist
            .iter()
            .enumerate()
            .map(|(idx, entry)| {
                let is_current = current_idx == Some(idx);
                // Marked rows render with the lighter marked highlight
                // (like the audio queue list); the row under the mouse
                // gets the hover highlight.
                let style = if self.video_marked.contains(idx) {
                    ctx.config.theme.marked_item_style
                } else if hover_idx == Some(idx) {
                    ctx.config.theme.hovered_item_style
                } else if is_current {
                    ctx.config.theme.current_item_style
                } else {
                    ctx.config.as_list_text_style()
                };
                let duration = entry
                    .duration
                    .map(|d| fmt.format(d as u64))
                    .unwrap_or_else(|| "-".to_owned());
                let prefix = if is_current { "❯ " } else { "  " };
                let mut title = entry.title.clone();
                truncate_to_width(&mut title, title_field.saturating_sub(2));
                let title_pad = title_field.saturating_sub(2 + title.width());
                let dur_pad = duration_w.saturating_sub(duration.width());
                ListItem::new(Line::styled(
                    format!(
                        "{prefix}{title}{} {}{duration}",
                        " ".repeat(title_pad),
                        " ".repeat(dur_pad),
                    ),
                    style,
                ))
            })
            .collect();

        let list = List::new(items)
            .highlight_style(if hover_idx == self.video_state.selected() {
                ctx.config.theme.hovered_item_style
            } else {
                ctx.config.theme.highlighted_item_style
            });
        StatefulWidget::render(list, area, frame.buffer_mut(), &mut self.video_state);

        if let Some(scrollbar) = ctx.config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            let max = self.video_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
            let position = self.video_state.offset().min(max);
            // content_length = max + 1 so the bottom position is reachable
            // (ratatui clamps positions to content_length - 1); the viewport
            // length keeps the thumb proportional to the visible rows.
            StatefulWidget::render(
                scrollbar,
                self.areas[Areas::Scrollbar],
                frame.buffer_mut(),
                &mut ratatui::widgets::ScrollbarState::new(max + 1)
                    .position(position)
                    .viewport_content_length(self.areas[Areas::Table].height as usize),
            );
        }
        Ok(())
    }

    /// Play the visible Video list from `idx` onwards: the entries are
    /// handed to mpv (a fresh instance when none runs, otherwise the
    /// running one is switched to them); neither the Jellyfin session
    /// playlist nor the persistent playlist is mutated.
    fn video_load_entry(&self, idx: usize, ctx: &Ctx) {
        let entries: Vec<crate::core::mpv::MpvPlaylistEntry> =
            if crate::core::mpv::session_playlist_shown(ctx) {
                ctx.mpv.playlist.borrow().iter().skip(idx).cloned().collect()
            } else {
                ctx.video_playlist.borrow().iter().skip(idx).cloned().collect()
            };
        if !entries.is_empty() {
            crate::core::mpv::play_video_entries(ctx, entries);
        }
    }

    /// Remove the entries at `indices` from the persistent video playlist
    /// and save it. The selection shifts up past the removed rows and the
    /// marks are dropped (their indices no longer exist).
    fn video_remove_entries(&mut self, indices: Vec<usize>, ctx: &Ctx) {
        if indices.is_empty() {
            return;
        }
        {
            let mut playlist = ctx.video_playlist.borrow_mut();
            for idx in indices.iter().rev() {
                if *idx < playlist.len() {
                    playlist.remove(*idx);
                }
            }
        }
        crate::ui::modals::paste::save_video_playlist(ctx);
        let len = ctx.video_playlist.borrow().len();
        self.video_items_len = len;
        self.video_marked.clear();
        self.video_marked.clear_anchor();
        if let Some(sel) = self.video_state.selected() {
            let removed_below = indices.iter().filter(|&&i| i < sel).count();
            let new_sel = sel.saturating_sub(removed_below);
            if len == 0 {
                self.video_state.select(None);
            } else {
                self.video_state.select(Some(new_sel.min(len - 1)));
            }
        }
    }
}

impl Pane for QueuePane {
    fn render(&mut self, frame: &mut Frame, area: Rect, ctx: &Ctx) -> anyhow::Result<()> {
        let Ctx { config, .. } = ctx;
        self.calculate_areas(area, ctx)?;

        match ctx.queue_tab.get() {
            // Video mode: show the persistent video playlist (the toggle is
            // drawn on the queue box's top border by TabScreen, after the
            // box block renders).
            crate::ctx::QueueTabMode::Video => return self.render_video(frame, ctx),
            // Chapters mode: show the chapter list instead of the song table.
            crate::ctx::QueueTabMode::Chapters if Self::chapters_available(ctx) => {
                return self.render_chapters(frame, ctx);
            }
            _ => {}
        }

        let filter_text = self.queue.filter_text(self.areas[Areas::Table].width, ctx);

        let table_block = {
            let border_style = config.as_border_style();
            let mut b = Block::default().border_style(border_style);
            if self.areas[Areas::FilterArea].height == 0
                && let Some(ref title) = filter_text
            {
                b = b.title(title.clone());
            }
            b
        };

        self.queue.state.set_content_and_viewport_len(
            self.queue.len(),
            self.areas[Areas::Table].height as usize,
        );

        // Mouse-over row highlight: the same selection highlight, slightly
        // brighter than the keyboard selection but dimmer than marked rows.
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            self.areas[Areas::Table],
            self.queue.state.offset(),
            self.queue.len(),
            1,
        );
        let row_highlight = if hover_idx == self.queue.state.get_selected() {
            config.theme.hovered_item_style
        } else {
            config.theme.current_item_style
        };

        let widths = Layout::horizontal(self.column_widths.as_slice())
            .flex(Flex::Start)
            .spacing(1)
            .split(self.areas[Areas::Table]);

        let formats = &config.theme.song_table_format;

        let new_album_indices: HashSet<usize> = self
            .queue
            .items
            .as_slice()
            .to_album_ranges()
            .map(|range| range.end.saturating_sub(1))
            .collect();
        let current_song_id = ctx.find_current_song_in_queue().map(|(_, song)| song.id);
        let marked = std::mem::take(self.queue.marked_mut());
        let filter = ctx.input.value(self.queue.filter_buffer_id);

        let table = VirtualizedTable::new(&self.queue.items)
            .column_widths(self.column_widths.clone())
            .row_highlight_style(row_highlight)
            .map_fn(|idx, song| {
                let is_current = current_song_id.is_some_and(|v| v == song.id);

                let is_marked = marked.contains(&idx);
                let is_hovered = hover_idx == Some(idx);
                // A resolved YouTube-style stream has no metadata of its
                // own; the columns show the cached info (like the MPRIS
                // tags: title in Title + Album, channel in Artist) so the
                // row is readable, not "Unknown".
                let yt = ctx.yt_info.borrow().get(&song.file).cloned();
                let columns = (0..formats.len()).map(|i| {
                    let mut max_len: usize = widths[i].width.into();
                    // The current song gets the same ❯ marker as the
                    // current chapter in the chapters list, in its first
                    // column (the title ellipsizes to make room).
                    let marker = (is_current && i == 0).then(|| {
                        max_len = max_len.saturating_sub(2);
                        Span::styled("❯ ", Style::default())
                    });

                    let mut line = if let Some(yt) = &yt {
                        stream_column_line(
                            &formats[i].prop,
                            yt,
                            max_len,
                            &config.theme.symbols,
                        )
                        .unwrap_or_else(|| {
                            song.as_line_ellipsized(
                                &formats[i].prop,
                                max_len,
                                &config.theme.symbols,
                                &config.theme.format_tag_separator,
                                config.theme.multiple_tag_resolution_strategy,
                                ctx,
                            )
                            .unwrap_or_default()
                        })
                    } else {
                        song.as_line_ellipsized(
                            &formats[i].prop,
                            max_len,
                            &config.theme.symbols,
                            &config.theme.format_tag_separator,
                            config.theme.multiple_tag_resolution_strategy,
                            ctx,
                        )
                        .unwrap_or_default()
                    };
                    if let Some(marker) = marker {
                        let mut spans = Vec::with_capacity(line.spans.len() + 1);
                        spans.push(marker);
                        spans.extend(line.spans);
                        line = Line::from(spans);
                    }
                    line.alignment(formats[i].alignment.into())
                });

                let is_matching_search = is_current
                    || if self.queue.filter_active {
                        song.matches(self.column_formats.as_slice(), &filter, ctx)
                    } else {
                        Default::default()
                    };

                let mut row = QueueRow::default();
                // Multi-selected rows get the lighter marked highlight (no
                // marker symbol); search matches keep the accent highlight;
                // the row under the mouse gets the hover highlight.
                if is_marked {
                    row.cell_style = Some(config.theme.marked_item_style);
                } else if is_hovered {
                    row.cell_style = Some(config.theme.hovered_item_style);
                } else if is_matching_search {
                    row.cell_style = Some(config.theme.highlighted_item_style);
                }

                let sep = ctx.config.theme.song_table_album_separator;
                if new_album_indices.contains(&idx)
                    && matches!(sep, AlbumSeparator::Underline)
                    && idx != self.queue.items.len().saturating_sub(1)
                {
                    row.underlined = true;
                }

                row.into_row(columns)
            });

        frame.render_widget(table_block, self.areas[Areas::Table]);
        frame.render_stateful_widget(table, self.areas[Areas::Table], &mut self.queue.state);

        let _ = std::mem::replace(self.queue.marked_mut(), marked);

        // Keep the selected song id in sync so the lyrics/info pane can show
        // its details while paused.
        ctx.queue_selected_id.set(self.queue.selected().map(|s| s.id));

        if let Some(scrollbar) = config.as_styled_scrollbar()
            && self.areas[Areas::Scrollbar].width > 0
        {
            frame.render_stateful_widget(
                scrollbar,
                self.areas[Areas::Scrollbar],
                self.queue.state.as_scrollbar_state_ref(),
            );
        }

        if let Some(filter_text) = filter_text
            && self.areas[Areas::FilterArea].height > 0
        {
            frame.render_widget(
                Line::from(filter_text).style(
                    config.theme.text_color.map(|c| Style::default().fg(c)).unwrap_or_default(),
                ),
                self.areas[Areas::FilterArea],
            );
        }

        Ok(())
    }

    fn calculate_areas(&mut self, area: Rect, ctx: &Ctx) -> Result<()> {
        let Ctx { config, .. } = ctx;

        let scrollbar_area_width: u16 = config.theme.scrollbar.is_some().into();

        let [table_area, scrollbar_area] = Layout::horizontal([
            Constraint::Percentage(100),
            Constraint::Length(scrollbar_area_width),
        ])
        .areas(area);

        let mut table_area = if self.queue.filter_active {
            self.areas[Areas::FilterArea] =
                Rect::new(table_area.x, table_area.y, table_area.width, 1);
            table_area.shrink_from_top(1)
        } else {
            self.areas[Areas::FilterArea] = Rect::default();
            table_area
        };

        // Create 1 column space between the table and the scrollbar
        table_area.width = table_area.width.saturating_sub(1);

        self.areas[Areas::Table] = table_area;
        self.areas[Areas::Scrollbar] = scrollbar_area;

        // The QueueHeaderPane's chapters header renders into this same width
        // so its Time / Duration labels line up with the chapter values.
        ctx.queue_table_width.set(Some(table_area.width));

        Ok(())
    }

    fn before_show(&mut self, ctx: &Ctx) -> Result<()> {
        self.queue.state.set_content_and_viewport_len(
            self.queue.len(),
            self.areas[Areas::Table].height as usize,
        );

        if self.should_center_cursor_on_current {
            let to_select = ctx
                .find_current_song_in_queue()
                .or(self.queue.selected_with_idx())
                .map(|(idx, _)| idx)
                .or(Some(0));
            self.queue.select_idx_opt(to_select, usize::MAX);
            self.should_center_cursor_on_current = false;
        } else {
            let to_select = self
                .queue
                .selected_with_idx()
                .or(ctx.find_current_song_in_queue())
                .map(|v| v.0)
                .or(Some(0));
            self.queue.select_idx_opt(to_select, usize::MAX);
        }

        // Chapters mode: land the highlight on the currently playing
        // chapter (the queue selection above already lands on the current
        // song).
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters {
            self.chapters_select_current(ctx);
        }
        // Video mode: land the highlight on the currently playing entry.
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video
            && let Some(idx) = if crate::core::mpv::session_playlist_shown(ctx) {
                ctx.mpv.playlist_pos.get()
            } else {
                crate::core::mpv::video_playlist_current_idx(ctx)
            }
            .filter(|i| {
                let len = if crate::core::mpv::session_playlist_shown(ctx) {
                    ctx.mpv.playlist.borrow().len()
                } else {
                    ctx.video_playlist.borrow().len()
                };
                *i < len
            })
        {
            self.video_state.select(Some(idx));
        }

        Ok(())
    }

    fn resize(&mut self, _area: Rect, ctx: &Ctx) -> Result<()> {
        self.queue.state.set_content_and_viewport_len(
            self.queue.len(),
            self.areas[Areas::Table].height as usize,
        );
        let to_select = self
            .queue
            .selected_with_idx()
            .or(ctx.find_current_song_in_queue())
            .map(|v| v.0)
            .or(Some(0));
        self.queue.select_idx_opt(to_select, ctx.config.scrolloff);
        ctx.render()?;
        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::Database => {
                self.queue.filter_active = false;
                self.queue.items.clone_from(&Self::local_queue(ctx));
                self.queue.unmark_all();
            }
            UiEvent::QueueChanged => {
                self.queue.items.clone_from(&Self::local_queue(ctx));
            }
            UiEvent::SongChanged => {
                if let Some((idx, _)) = ctx.find_current_song_in_queue()
                    && ctx.config.select_current_song_on_change
                {
                    match (is_visible, ctx.config.center_current_song_on_change) {
                        (true, true) => {
                            self.queue.select_idx(idx, usize::MAX);
                        }
                        (false, true) => {
                            self.queue.select_idx(idx, usize::MAX);
                            self.should_center_cursor_on_current = true;
                        }
                        (true, false) | (false, false) => {
                            self.queue.select_idx(idx, ctx.config.scrolloff);
                        }
                    }

                    ctx.render()?;
                }
            }
            UiEvent::Reconnected => {
                self.before_show(ctx)?;
            }
            UiEvent::ConfigChanged => {
                let (column_widths, column_formats) = Self::init(ctx);
                self.column_formats = column_formats;
                self.column_widths = column_widths;
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        let position = event.into();

        // Audio / Video / Chapters toggle clicks.
        if matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::DoubleClick)
            && self
                .toggle_areas
                .iter()
                .any(|area| area.contains(position))
        {
            let mode = if self.toggle_areas[1].contains(position) {
                crate::ctx::QueueTabMode::Video
            } else if self.toggle_areas[2].contains(position) {
                crate::ctx::QueueTabMode::Chapters
            } else {
                crate::ctx::QueueTabMode::Audio
            };
            Self::set_tab(self, ctx, mode);
            ctx.render()?;
            return Ok(());
        }

        // Chapters mode: a single click only highlights (never plays, even
        // when the row is already highlighted by keyboard navigation); a
        // double click seeks to the chapter. The wheel moves the highlight.
        if Self::chapters_available(ctx)
            && ctx.queue_tab.get() == crate::ctx::QueueTabMode::Chapters
        {            let table = self.areas[Areas::Table];
            if table.contains(position) {
                match event.kind {
                    MouseEventKind::LeftClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.chapters_state.offset() + row;
                        if idx < self.chapters_items_len {
                            self.chapters_state.select(Some(idx));
                            ctx.render()?;
                        }
                        return Ok(());
                    }
                    MouseEventKind::DoubleClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.chapters_state.offset() + row;
                        let chapters = Self::current_chapters(ctx);
                        if let Some(chapter) = chapters.get(idx) {
                            self.seek_to(chapter.start_secs, ctx);
                            self.chapters_state.select(None);
                            ctx.render()?;
                        }
                        return Ok(());
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        // Scroll moves the highlight (first move selects
                        // chapter 0), like w/s; the offset follows it.
                        let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                        let current = self.chapters_state.selected().unwrap_or(0) as i64;
                        let len = self.chapters_items_len;
                        if len == 0 {
                            return Ok(());
                        }
                        let new = (current + dir).clamp(0, len as i64 - 1) as usize;
                        if new != self.chapters_state.selected().unwrap_or(usize::MAX) {
                            self.chapters_state.select(Some(new));
                            ctx.render()?;
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        // Video mode: a single click highlights an entry, a double click
        // loads it in mpv (the list opens with the playing entry already
        // highlighted, so a plain click on it must never reload the
        // stream), wheel to scroll.
        if ctx.queue_tab.get() == crate::ctx::QueueTabMode::Video {
            let table = self.areas[Areas::Table];
            if table.contains(position) {
                match event.kind {
                    // A double click loads the entry (and drops the
                    // highlight so the next click re-highlights instead of
                    // loading again).
                    MouseEventKind::DoubleClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            self.video_load_entry(idx, ctx);
                            self.video_state.select(None);
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    // A single click only highlights — never loads. A
                    // ctrl+click toggles the mark, an alt+click range-marks
                    // from the anchor (like the audio queue list); a plain
                    // click on a different row drops the multi-selection.
                    MouseEventKind::LeftClick
                        if event.modifiers.contains(
                            crossterm::event::KeyModifiers::CONTROL,
                        ) =>
                    {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            self.video_state.select(Some(idx));
                            self.video_marked.toggle(idx);
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::LeftClick
                        if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
                    {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            if self.video_marked.anchor().is_none() {
                                self.video_marked.set_anchor(idx);
                            }
                            // Replace the previous alt/shift range, so
                            // alt+clicking closer to the anchor deselects
                            // the entries beyond it.
                            self.video_marked.select_range(idx);
                            self.video_state.select(Some(idx));
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::LeftClick => {
                        let row = usize::from(position.y.saturating_sub(table.y));
                        let idx = self.video_state.offset() + row;
                        if idx < self.video_items_len {
                            // A plain click on a different row drops the
                            // multi-selection; clicking a marked row keeps
                            // it.
                            if !self.video_marked.is_empty()
                                && Some(idx) != self.video_state.selected()
                            {
                                self.video_marked.clear();
                            }
                            self.video_state.select(Some(idx));
                            self.video_marked.set_anchor(idx);
                            self.video_marked.clear_range();
                            ctx.render()?;
                            return Ok(());
                        }
                    }
                    MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                        // Wheel moves the highlight (like w/s), honoring
                        // the configured scroll amount; the viewport
                        // follows it.
                        let dir =
                            if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                        let amount = ctx.config.scroll_amount.max(1) as i64;
                        self.video_move(dir * amount, ctx)?;
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

        // The scrollbar serves whichever list the Queue tab is showing
        // (audio queue / video list / chapters), and a thumb drag keeps the
        // thumb under the pointer 1:1.
        if let Some(scrollbar_area) = self.scrollbar_area()
            && ctx.config.theme.scrollbar.is_some()
            && matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. })
        {
            let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
            let mode = ctx.queue_tab.get();
            let viewport = self.areas[Areas::Table].height as usize;
            let (content_len, viewport_len, position) = match mode {
                crate::ctx::QueueTabMode::Video => (
                    self.video_items_len.saturating_sub(viewport).saturating_add(1).max(1),
                    viewport,
                    self.video_state.offset(),
                ),
                crate::ctx::QueueTabMode::Chapters => (
                    self.chapters_items_len.saturating_sub(viewport).saturating_add(1).max(1),
                    viewport,
                    self.chapters_state.offset(),
                ),
                crate::ctx::QueueTabMode::Audio => {
                    let viewport = self
                        .queue
                        .state
                        .viewport_len()
                        .unwrap_or(scrollbar_area.height as usize);
                    (
                        self.queue.items.len().saturating_sub(viewport).saturating_add(1).max(1),
                        viewport,
                        self.queue.state.inner.offset(),
                    )
                }
            };
            let drag = match mode {
                crate::ctx::QueueTabMode::Video => &mut self.video_scrollbar_drag,
                crate::ctx::QueueTabMode::Chapters => &mut self.chapters_scrollbar_drag,
                crate::ctx::QueueTabMode::Audio => &mut self.queue.state.scrollbar_drag,
            };
            if let Some(perc) = drag.handle(
                event,
                scrollbar_area,
                content_len,
                viewport_len,
                position,
                begin_len,
                end_len,
            ) {
                match mode {
                    crate::ctx::QueueTabMode::Video => self.video_scroll_to(perc, ctx),
                    crate::ctx::QueueTabMode::Chapters => self.chapters_scroll_to(perc, ctx),
                    crate::ctx::QueueTabMode::Audio => {
                        self.queue.state.scroll_to(perc, ctx.config.scrolloff);
                    }
                }
                ctx.render()?;
                return Ok(());
            }
        }

        if !self.areas[Areas::Table].contains(position) {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick
                if self.areas[Areas::Table].contains(event.into())
                    && event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) =>
            {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    self.queue.state.toggle_mark(idx);

                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick
                if self.areas[Areas::Table].contains(event.into())
                    && event.modifiers.contains(crossterm::event::KeyModifiers::ALT) =>
            {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    if self.queue.state.mark_anchor().is_none() {
                        self.queue.state.set_mark_anchor(idx);
                    }
                    let anchor = self.queue.state.mark_anchor().unwrap_or(idx);
                    // Replace the previous alt/shift range, so alt+clicking
                    // closer to the anchor deselects the items beyond it,
                    // just like backing up with Shift+Up.
                    if let Some((lo, hi)) = self.queue.state.take_range_mark() {
                        for i in lo..=hi {
                            self.queue.state.marked.remove(&i);
                        }
                    }
                    let (lo, hi) = (anchor.min(idx), anchor.max(idx));
                    if lo < hi {
                        self.queue.state.mark_range(lo, hi);
                        self.queue.state.set_range_mark(lo, hi);
                    }
                    // lo == hi means the anchor itself was clicked: the old
                    // range was already unmarked, so everything (including
                    // the anchor) is deselected.
                    self.queue.select_idx(idx, ctx.config.scrolloff);

                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick if self.areas[Areas::Table].contains(event.into()) => {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    // A plain click on a different row drops the
                    // multi-selection (ctrl/alt clicks above keep their
                    // marking behavior). Clicking the selected row keeps it.
                    if !self.queue.state.marked.is_empty()
                        && Some(idx) != self.queue.state.get_selected()
                    {
                        self.queue.state.unmark_all();
                    }
                    self.queue.select_idx(idx, ctx.config.scrolloff);
                    self.queue.state.set_mark_anchor(idx);
                    self.queue.state.clear_range_mark();

                    ctx.render()?;
                }
            }
            MouseEventKind::LeftClick => {}
            MouseEventKind::DoubleClick if self.areas[Areas::Table].contains(event.into()) => {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();

                if let Some(song) = self
                    .queue
                    .state
                    .get_at_rendered_row(clicked_row)
                    .and_then(|idx| self.queue.items.get(idx))
                {
                    play_queue_song(song, ctx);
                }
            }
            MouseEventKind::DoubleClick => {}
            MouseEventKind::MiddleClick if self.areas[Areas::Table].contains(event.into()) => {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();

                if let Some(selected_song) = self
                    .queue
                    .state
                    .get_at_rendered_row(clicked_row)
                    .and_then(|idx| self.queue.items.get(idx))
                {
                    let id = selected_song.id;
                    ctx.command(move |client| {
                        client.delete_id(id)?;
                        Ok(())
                    });
                }
            }
            MouseEventKind::MiddleClick => {}
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
                if self.areas[Areas::Table].contains(event.into()) =>
            {
                // Wheel moves the highlight (like w/s), honoring the
                // configured scroll amount; the viewport follows it.
                let len = self.queue.items.len();
                if len == 0 {
                    return Ok(());
                }
                let dir = if matches!(event.kind, MouseEventKind::ScrollUp) { -1 } else { 1 };
                let amount = ctx.config.scroll_amount.max(1) as i64;
                let current = i64::try_from(self.queue.state.get_selected().unwrap_or(0)).unwrap_or(0);
                let new = (current + dir * amount).clamp(0, i64::try_from(len - 1).unwrap_or(0)) as usize;
                if new != self.queue.state.get_selected().unwrap_or(usize::MAX) {
                    self.queue.select_idx(new, ctx.config.scrolloff);
                    ctx.render()?;
                }
                return Ok(());
            }
            MouseEventKind::ScrollDown | MouseEventKind::ScrollUp => {}
            MouseEventKind::RightClick if self.areas[Areas::Table].contains(event.into()) => {
                let clicked_row: usize = event.y.saturating_sub(self.areas[Areas::Table].y).into();
                if let Some(idx) = self.queue.state.get_at_rendered_row(clicked_row) {
                    self.queue.select_idx(idx, ctx.config.scrolloff);

                    ctx.render()?;
                }
                self.open_context_menu(ctx);
            }
            MouseEventKind::RightClick => {}
            MouseEventKind::Drag { .. } => {}
            MouseEventKind::LeftRelease => {}
            MouseEventKind::Moved => {}
        }

        Ok(())
    }

    fn on_query_finished(
        &mut self,
        id: &'static str,
        data: MpdQueryResult,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
        match (id, data) {
            (FILE_CHAPTERS, MpdQueryResult::Any(any)) => {
                if let Ok(boxed) = any.downcast::<(
                    String,
                    Result<Vec<crate::shared::chapters::Chapter>, String>,
                )>() {
                    let (file, result) = *boxed;
                    if let Ok(chapters) = result
                        && !chapters.is_empty()
                    {
                        ctx.chapters.borrow_mut().insert(file, chapters);
                        // The current song just gained markers: auto-open
                        // the Chapters list (gated by the setting; the
                        // active tab is never switched).
                        ctx.auto_show_chapters();
                    }
                    ctx.render()?;
                }
            }
            (ADD_TO_PLAYLIST, MpdQueryResult::AddToPlaylist { playlists, song_file }) => {
                modal!(
                    ctx,
                    SelectModal::builder()
                        .ctx(ctx)
                        .options(playlists)
                        .confirm_label("Add")
                        .title("Select a playlist")
                        .on_confirm(move |ctx, selected, _idx| {
                            let song_file = song_file.clone();
                            ctx.command(move |client| {
                                if song_file.starts_with('/') {
                                    client.add_to_playlist(
                                        &selected,
                                        &format!("file://{song_file}"),
                                        None,
                                    )?;
                                } else {
                                    client.add_to_playlist(&selected, &song_file, None)?;
                                }
                                status_info!("Song added to playlist {}", selected);
                                Ok(())
                            });
                            Ok(())
                        })
                        .build()
                );
            }
            (
                ADD_TO_PLAYLIST_MULTIPLE,
                MpdQueryResult::AddToPlaylistMultiple { playlists, song_files },
            ) => {
                modal!(
                    ctx,
                    SelectModal::builder()
                        .ctx(ctx)
                        .options(playlists)
                        .confirm_label("Add")
                        .title("Select a playlist")
                        .on_confirm(move |ctx, selected, _idx| {
                            ctx.command(move |client| {
                                let songs_len = song_files.len();
                                for song_file in song_files {
                                    if song_file.starts_with('/') {
                                        client.add_to_playlist(
                                            &selected,
                                            &format!("file://{song_file}"),
                                            None,
                                        )?;
                                    } else {
                                        client.add_to_playlist(&selected, &song_file, None)?;
                                    }
                                }
                                status_info!("{} songs added to playlist {}", songs_len, selected);
                                Ok(())
                            });
                            Ok(())
                        })
                        .build()
                );
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        match kind {
            InputResultEvent::Push => {
                self.queue.recalculate_matched_items(self.column_formats.as_slice(), ctx);
                self.queue.jump_first_matching(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Pop => {
                self.queue.recalculate_matched_items(self.column_formats.as_slice(), ctx);
            }
            InputResultEvent::Confirm => {}
            InputResultEvent::Cancel => {
                self.queue.set_filter_active(false);
                ctx.input.clear_buffer(self.queue.filter_buffer_id);
            }
            InputResultEvent::NoChange => {}
        }
        ctx.render()?;
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        // Chapters / Video modes: the keyboard drives those lists (w/s/↑/↓
        // move the highlight, d/→/Enter activate, PageUp/PageDown page).
        match ctx.queue_tab.get() {
            crate::ctx::QueueTabMode::Chapters if Self::chapters_available(ctx) => {
                return self.handle_chapters_action(event, ctx);
            }
            crate::ctx::QueueTabMode::Video => return self.handle_video_action(event, ctx),
            _ => {}
        }
        // w/s/d/→ navigate and play like the Radio tab: w/s move up/down the
        // queue, d and → play the highlighted track.
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => {
                    if !self.queue.is_empty() {
                        self.queue.prev(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                    Ok(())
                }
                DirectoriesActions::FolderDown => {
                    if !self.queue.is_empty() {
                        self.queue.next(ctx.config.scrolloff, ctx.config.wrap_navigation);
                    }
                    ctx.render()?;
                    Ok(())
                }
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if let Some(selected_song) = self.queue.selected() {
                        play_queue_song(selected_song, ctx);
                    }
                    Ok(())
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::Delete if !self.queue.marked().is_empty() => {
                    for range in self.queue.marked().ranges().rev() {
                        ctx.command(move |client| {
                            client.delete_from_queue(range.into())?;
                            Ok(())
                        });
                    }
                    self.queue.marked_mut().clear();
                    self.queue.state.clear_mark_anchor();
                    status_info!("Marked songs removed from queue");
                    ctx.render()?;
                }
                QueueActions::Delete => {
                    if let Some(selected_song) = self.queue.selected() {
                        let id = selected_song.id;
                        ctx.command(move |client| {
                            client.delete_id(id)?;
                            Ok(())
                        });
                    } else {
                        status_error!("No song selected");
                    }
                }
                QueueActions::DeleteAll => {
                    modal!(
                        ctx,
                        ConfirmModal::builder()
                            .ctx(ctx)
                            .message(vec![
                                "Are you sure you want to clear the queue?",
                                "This action cannot be undone."
                            ])
                            .action(Action::Single {
                                on_confirm: Box::new(|ctx| {
                                    ctx.command(|client| Ok(client.clear()?));
                                    Ok(())
                                }),
                                confirm_label: Some("Clear"),
                                cancel_label: None,
                            })
                            .size((45, 6))
                            .build()
                    );
                }
                QueueActions::Play => {
                    if let Some(selected_song) = self.queue.selected() {
                        play_queue_song(selected_song, ctx);
                    }
                }
                QueueActions::ToggleChapters => {
                    // Cycle the list view: Audio -> Video -> Chapters ->
                    // Audio (Chapters only when the track has markers).
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    if let Some((idx, _)) = ctx.status.songid.and_then(|id| {
                        self.queue.items.iter().enumerate().find(|(_, song)| song.id == id)
                    }) {
                        let scrolloff =
                            if self.queue.selected_with_idx().is_some_and(|(i, _)| i == idx) {
                                usize::MAX
                            } else {
                                ctx.config.scrolloff
                            };
                        self.queue.select_idx(idx, scrolloff);
                        ctx.render()?;
                    } else {
                        status_info!("No song is currently playing");
                    }
                }
                QueueActions::Shuffle if !self.queue.marked().is_empty() => {
                    for range in self.queue.marked().ranges().rev() {
                        ctx.command(move |client| {
                            client.shuffle(Some(range.into()))?;
                            Ok(())
                        });
                    }
                    status_info!("Shuffled selected songs");
                }
                QueueActions::Shuffle => {
                    ctx.command(move |client| {
                        client.shuffle(None)?;
                        Ok(())
                    });
                    status_info!("Shuffled the queue");
                }
                QueueActions::SortByColumn(idx) => {
                    QueueHeaderPane::sort_by_column(self.column_formats.as_slice(), *idx, ctx)?;
                    ctx.render()?;
                }
                QueueActions::Unused => {}
            }
        } else if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            // Audio mode: the queue list reuses the shared SongListCore
            // arms (navigation, half/page/top/bottom, filter + jump-
            // matching, range-select, invert, esc-deselect, rate, save,
            // delete-from-playlist). Queue-specific semantics stay here:
            match action {
                CommonAction::Select => {
                    // Space: play/pause the currently highlighted track.
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.pause_toggle()?;
                            Ok(())
                        });
                    } else {
                        ctx.command(move |client| {
                            client.play()?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                CommonAction::MoveUp if !self.queue.marked().is_empty() => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }

                    if let Some(0) = self.queue.marked().first() {
                        return Ok(());
                    }

                    let ranges = self.queue.marked().ranges().collect_vec();
                    for range in ranges {
                        for idx in range.clone() {
                            let new_idx = idx.saturating_sub(1);
                            self.queue.items.swap(idx, new_idx);
                        }

                        let new_start_idx = range.start().saturating_sub(1);
                        ctx.command(move |client| {
                            client.move_in_queue(
                                range.into(),
                                QueuePosition::Absolute(new_start_idx),
                            )?;
                            Ok(())
                        });
                    }

                    if let Some(start) = self.queue.marked().first() {
                        let new_idx = start.saturating_sub(1);
                        self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    }

                    let mut new_marked =
                        self.queue.marked().iter().map(|i| i.saturating_sub(1)).collect();
                    std::mem::swap(self.queue.marked_mut(), &mut new_marked);

                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::MoveDown if !self.queue.marked().is_empty() => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }

                    if let Some(last_idx) = self.queue.marked().last()
                        && *last_idx == self.queue.len() - 1
                    {
                        return Ok(());
                    }

                    let ranges = self.queue.marked().ranges().rev().collect_vec();
                    for range in ranges {
                        for idx in range.clone().rev() {
                            let new_idx = idx.saturating_add(1);
                            self.queue.items.swap(idx, new_idx);
                        }

                        let new_start_idx = range.start().saturating_add(1);
                        ctx.command(move |client| {
                            client.move_in_queue(
                                range.into(),
                                QueuePosition::Absolute(new_start_idx),
                            )?;
                            Ok(())
                        });
                    }

                    if let Some(start) = self.queue.marked().last() {
                        let new_idx = start.saturating_add(1);
                        self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    }

                    let mut new_marked =
                        self.queue.marked().iter().map(|i| i.saturating_add(1)).collect();
                    std::mem::swap(self.queue.marked_mut(), &mut new_marked);

                    ctx.render()?;
                    return Ok(());
                }
                CommonAction::MoveUp => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }

                    let Some((idx, selected)) = self.queue.selected_with_idx() else {
                        return Ok(());
                    };

                    let new_idx = idx.saturating_sub(1);
                    let id = selected.id;
                    ctx.command(move |client| {
                        client.move_id(id, QueuePosition::Absolute(new_idx))?;
                        Ok(())
                    });
                    self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    self.queue.items.swap(idx, new_idx);
                    ctx.render()?;
                }
                CommonAction::MoveDown => {
                    if self.queue.is_empty() {
                        return Ok(());
                    }

                    let Some((idx, selected)) = self.queue.selected_with_idx() else {
                        return Ok(());
                    };

                    let new_idx = (idx + 1).min(self.queue.len() - 1);
                    let id = selected.id;
                    ctx.command(move |client| {
                        client.move_id(id, QueuePosition::Absolute(new_idx))?;
                        Ok(())
                    });
                    self.queue.select_idx(new_idx, ctx.config.scrolloff);
                    self.queue.items.swap(idx, new_idx);
                    ctx.render()?;
                }
                CommonAction::Delete => {
                    // `Del` removes the highlighted song (or every marked
                    // song), like the `x` key — same as the context menu's
                    // Remove.
                    if !self.queue.marked().is_empty() {
                        for range in self.queue.marked().ranges().rev() {
                            ctx.command(move |client| {
                                client.delete_from_queue(range.into())?;
                                Ok(())
                            });
                        }
                        self.queue.marked_mut().clear();
                        self.queue.state.clear_mark_anchor();
                        status_info!("Marked songs removed from queue");
                    } else if let Some(selected_song) = self.queue.selected() {
                        let id = selected_song.id;
                        ctx.command(move |client| {
                            client.delete_id(id)?;
                            Ok(())
                        });
                    } else {
                        status_error!("No song selected");
                    }
                    ctx.render()?;
                }
                CommonAction::AddOptions { kind: AddKind::Action(options) } => {
                    let (enqueue, _hovered_song_idx) = self.enqueue_items(options.all);

                    if !enqueue.is_empty() {
                        Client::resolve_and_enqueue(
                            ctx,
                            enqueue,
                            options.position,
                            AutoplayKind::None,
                            None,
                            None,
                        );
                        self.queue.marked_mut().clear();
                    }
                }
                CommonAction::AddOptions { kind: AddKind::Modal(items) } => {
                    let opts = items
                        .into_iter()
                        .map(|(label, mut opts)| {
                            opts.autoplay = AutoplayKind::None;
                            let (enqueue, hovered_song_idx) = self.enqueue_items(opts.all);
                            (label, opts, (enqueue, hovered_song_idx))
                        })
                        .collect_vec();

                    modal!(ctx, create_add_modal(opts, ctx));
                    self.queue.marked_mut().clear();
                }
                CommonAction::ShowInfo => {
                    if let Some(selected_song) = self.queue.selected() {
                        modal!(
                            ctx,
                            InfoListModal::builder()
                                .items(selected_song)
                                .title("Song info")
                                .column_widths(&[30, 70])
                                .build()
                        );
                    } else {
                        status_error!("No song selected");
                    }
                }
                CommonAction::Confirm => {
                    // Enter opens the context menu (like right-click);
                    // `d`/`→` still play the highlighted track.
                    self.open_context_menu(ctx);
                }
                CommonAction::ContextMenu => {
                    self.open_context_menu(ctx);
                }
                CommonAction::Right | CommonAction::Left => {}
                other => self.handle_claimed_common_action(other, event, ctx)?,
            }
        } else if let Some(action) = event.claim_global() {
            match action {
                // < / > seek the playing track (instead of prev/next track).
                GlobalAction::PreviousTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Decrease(5))?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                GlobalAction::NextTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Increase(5))?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                GlobalAction::ExternalCommand { command, .. } => {
                    let songs =
                        create_env(ctx, self.items(false).map(|(_, song)| song.file.as_str()));
                    run_external(command.clone(), songs);
                }
                _ => {
                    event.abandon();
                }
            }
        }

        Ok(())
    }
}

impl SongListCore<Song, TableState> for QueuePane {
    fn list(&self) -> &Dir<Song, TableState> {
        &self.queue
    }

    fn list_mut(&mut self) -> &mut Dir<Song, TableState> {
        &mut self.queue
    }

    fn list_songs_in_item(
        &self,
        item: Song,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
        move |_client| Ok(vec![item])
    }

    /// The queue filter jump-matching uses the queue's own column formats
    /// (not the generic browser song format).
    fn song_format(&self, _ctx: &Ctx) -> Vec<Property<SongProperty>> {
        self.column_formats.clone()
    }
}

impl QueuePane {
    /// Keyboard handling for Chapters mode: navigate the chapter list
    /// (w/s/↑/↓, PageUp/PageDown, Home/End) and play a chapter with
    /// d/→/Enter. `c` still toggles back to the queue.
    fn handle_chapters_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => self.chapters_move(-1, ctx),
                DirectoriesActions::FolderDown => self.chapters_move(1, ctx),
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.chapters_play_selected(ctx)
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::ToggleChapters => {
                    // Cycle back to the Audio view.
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    let chapters = Self::current_chapters(ctx);
                    let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
                        ctx.mpv.position
                    } else {
                        ctx.status.elapsed.as_secs_f64()
                    };
                    let idx = chapters
                        .iter()
                        .rposition(|c| position >= c.start_secs)
                        .unwrap_or(0);
                    self.chapters_jump(idx, ctx)?;
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            match action {
                CommonAction::Up => self.chapters_move(-1, ctx)?,
                CommonAction::Down => self.chapters_move(1, ctx)?,
                CommonAction::PageUp => self.chapters_page(-1, ctx)?,
                CommonAction::PageDown => self.chapters_page(1, ctx)?,
                CommonAction::Top => self.chapters_jump(0, ctx)?,
                CommonAction::Bottom => self.chapters_jump(usize::MAX, ctx)?,
                CommonAction::Confirm => self.chapters_play_selected(ctx)?,
                CommonAction::Select => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.pause_toggle()?;
                            Ok(())
                        });
                    } else {
                        ctx.command(move |client| {
                            client.play()?;
                            Ok(())
                        });
                    }
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        if let Some(action) = event.claim_global() {
            match action {
                // < / > seek the playing track (like the queue view).
                GlobalAction::PreviousTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Decrease(5))?;
                            Ok(())
                        });
                    }
                }
                GlobalAction::NextTrack => {
                    if matches!(ctx.status.state, State::Play | State::Pause) {
                        ctx.command(move |client| {
                            client.seek_current(ValueChange::Increase(5))?;
                            Ok(())
                        });
                    }
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        Ok(())
    }

    /// Keyboard handling for Video mode: navigate the mpv playlist with
    /// w/s/↑/↓, load an entry with d/→/Enter. `c` cycles back to Audio.
    fn handle_video_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        if let Some(action) = event.claim_directories() {
            return match action {
                DirectoriesActions::FolderUp => self.video_move(-1, ctx),
                DirectoriesActions::FolderDown => self.video_move(1, ctx),
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    self.video_play_selected(ctx)
                }
                DirectoriesActions::FolderCollapse => Ok(()),
            };
        }
        if let Some(action) = event.claim_queue() {
            match action {
                QueueActions::ToggleChapters => {
                    self.cycle_tab(ctx);
                    ctx.render()?;
                }
                QueueActions::JumpToCurrent => {
                    let idx = if crate::core::mpv::session_playlist_shown(ctx) {
                        ctx.mpv.playlist_pos.get()
                    } else {
                        crate::core::mpv::video_playlist_current_idx(ctx)
                    };
                    if let Some(idx) = idx.filter(|i| *i < self.video_items_len) {
                        self.video_jump(idx, ctx)?;
                    }
                }
                _ => {}
            }
            return Ok(());
        }
        if let Some(action) = event.claim_common().map(|v| v.to_owned()) {
            match action {
                CommonAction::Up => self.video_move(-1, ctx)?,
                CommonAction::Down => self.video_move(1, ctx)?,
                CommonAction::PageUp => self.video_page(-1, ctx)?,
                CommonAction::PageDown => self.video_page(1, ctx)?,
                CommonAction::Top => self.video_jump(0, ctx)?,
                CommonAction::Bottom => self.video_jump(usize::MAX, ctx)?,
                // Enter opens the context menu (like right-click);
                // `d`/`→` still load the highlighted entry.
                CommonAction::Confirm => self.open_context_menu(ctx),
                CommonAction::SelectUp | CommonAction::SelectDown => {
                    // Shift+Up/Down: range-select from the anchor (set by
                    // plain clicks / the first shift-press), moving first
                    // so the newly reached row is included; each press
                    // replaces the previous range.
                    let dir = if matches!(action, CommonAction::SelectDown) { 1 } else { -1 };
                    let start = self.video_state.selected().unwrap_or(0);
                    if self.video_marked.anchor().is_none() || self.video_marked.is_empty() {
                        self.video_marked.set_anchor(start);
                    }
                    self.video_move(dir, ctx)?;
                    let sel = self.video_state.selected().unwrap_or(start);
                    self.video_marked.select_range(sel);
                    ctx.render()?;
                }
                CommonAction::Delete => {
                    // Remove the marked entries (or the highlighted one)
                    // from the persistent video playlist (a live session
                    // keeps playing them; the queue no longer contains
                    // them). The Jellyfin session's own playlist is live
                    // mpv state — never deletable.
                    if !crate::core::mpv::session_playlist_shown(ctx) {
                        let indices: Vec<usize> = if self.video_marked.is_empty() {
                            self.video_state.selected().into_iter().collect()
                        } else {
                            self.video_marked.iter().collect()
                        };
                        if !indices.is_empty() {
                            self.video_remove_entries(indices, ctx);
                            ctx.render()?;
                        }
                    }
                }
                CommonAction::Select => {
                    // Toggle the video's pause.
                    if let Some(socket) = ctx.mpv.socket.clone() {
                        crate::core::mpv::mpv_toggle_pause(&socket);
                    }
                }
                CommonAction::Close if !self.video_marked.is_empty() => {
                    self.video_marked.clear();
                    self.video_marked.clear_anchor();
                    // Esc is bound to both Close and ShowSettings: clearing a
                    // selection consumes the keypress, so the settings panel
                    // only opens on a second Esc (when nothing is selected).
                    event.consume();
                    ctx.render()?;
                }
                _ => event.abandon(),
            }
            return Ok(());
        }
        event.abandon();
        Ok(())
    }

    /// Move the video list highlight by `dir` rows (clamped). The first
    /// move from no selection highlights the first entry (menu convention).
    fn video_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let Some(current) = self.video_state.selected() else {
            self.video_state.select(Some(0));
            ctx.render()?;
            return Ok(());
        };
        let new = ((current as i64) + dir).clamp(0, len as i64 - 1) as usize;
        if new != current {
            self.video_state.select(Some(new));
            ctx.render()?;
        }
        Ok(())
    }

    /// Scroll the video list to a scrollbar fraction (0.0..=1.0): the
    /// offset lands so the thumb matches the pointer. `max` mirrors the
    /// renderer's `items_len - table_height`.
    fn video_scroll_to(&mut self, perc: f64, ctx: &Ctx) {
        let max = self.video_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
        let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
        let _ = self.video_jump(new.min(max), ctx);
    }

    /// Scroll the chapters list to a scrollbar fraction (0.0..=1.0).
    fn chapters_scroll_to(&mut self, perc: f64, ctx: &Ctx) {
        let max =
            self.chapters_items_len.saturating_sub(self.areas[Areas::Table].height as usize);
        let new = ((perc.clamp(0.0, 1.0)) * max as f64).floor() as usize;
        let _ = self.chapters_jump(new.min(max), ctx);
    }

    /// Page the video list by one viewport in `dir` direction.
    fn video_page(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let viewport = self.areas[Areas::Table].height.max(1) as i64;
        self.video_move(dir * viewport, ctx)
    }

    /// Highlight the playlist entry at `idx` (clamped to the list).
    fn video_jump(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let len = self.video_items_len;
        if len == 0 {
            return Ok(());
        }
        let idx = idx.min(len - 1);
        if self.video_state.selected() != Some(idx) {
            self.video_state.select(Some(idx));
            ctx.render()?;
        }
        Ok(())
    }

    /// Load the highlighted playlist entry in mpv (the current view's
    /// equivalent of playing a song).
    fn video_play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        if let Some(idx) = self.video_state.selected() {
            self.video_load_entry(idx, ctx);
        }
        Ok(())
    }

    /// Move the chapters highlight by `dir` rows (clamped). The first move
    /// from no selection highlights the first chapter (menu convention).
    fn chapters_move(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let len = self.chapters_items_len;
        if len == 0 {
            return Ok(());
        }
        let Some(current) = self.chapters_state.selected() else {
            self.chapters_state.select(Some(0));
            ctx.render()?;
            return Ok(());
        };
        let new = ((current as i64) + dir).clamp(0, len as i64 - 1) as usize;
        if new != current {
            self.chapters_state.select(Some(new));
            ctx.render()?;
        }
        Ok(())
    }

    /// Page the chapters list by one viewport in `dir` direction.
    fn chapters_page(&mut self, dir: i64, ctx: &Ctx) -> Result<()> {
        let viewport = self.areas[Areas::Table].height.max(1) as i64;
        self.chapters_move(dir * viewport, ctx)
    }

    /// Highlight the chapter at `idx` (clamped to the list).
    fn chapters_jump(&mut self, idx: usize, ctx: &Ctx) -> Result<()> {
        let len = self.chapters_items_len;
        if len == 0 {
            return Ok(());
        }
        let idx = idx.min(len - 1);
        if self.chapters_state.selected() != Some(idx) {
            self.chapters_state.select(Some(idx));
            ctx.render()?;
        }
        Ok(())
    }

    /// Select the chapter currently playing, used when the chapters view
    /// opens (startup, tab re-entry, toggling) so the highlight lands on
    /// the track's current position.
    fn chapters_select_current(&mut self, ctx: &Ctx) {
        let chapters = Self::current_chapters(ctx);
        if chapters.is_empty() {
            return;
        }
        let position = if crate::core::mpv::mpv_is_ui_source(ctx) {
            ctx.mpv.position
        } else {
            ctx.status.elapsed.as_secs_f64()
        };
        let idx = chapters
            .iter()
            .rposition(|c| position >= c.start_secs)
            .unwrap_or(0);
        self.chapters_state.select(Some(idx));
    }

    /// Seek to the highlighted chapter (MPD or mpv). The highlight stays
    /// put so keyboard navigation continues from the played chapter (the
    /// mouse's click-highlight-then-click-again behavior lives in
    /// `handle_mouse_event`).
    fn chapters_play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(idx) = self.chapters_state.selected() else { return Ok(()) };
        let chapters = Self::current_chapters(ctx);
        if let Some(chapter) = chapters.get(idx) {
            self.seek_to(chapter.start_secs, ctx);
        }
        Ok(())
    }

    fn scrollbar_area(&self) -> Option<Rect> {
        let area = self.areas[Areas::Scrollbar];
        if area.width > 0 { Some(area) } else { None }
    }
}

/// Truncate `s` so its display width fits `max_cols`, keeping whole
/// graphemes (wide glyphs take two columns, so a grapheme count is not
/// enough to keep the following columns in place).
fn truncate_to_width(s: &mut String, max_cols: usize) {
    if s.width() <= max_cols {
        return;
    }
    let mut out = String::new();
    let mut used = 0;
    for grapheme in s.graphemes(true) {
        let w = grapheme.width();
        if used + w > max_cols {
            break;
        }
        out.push_str(grapheme);
        used += w;
    }
    *s = out;
}

/// The queue-table cell of a resolved YouTube-style stream for the Title /
/// Album / Artist columns: the cached info (title in Title + Album,
/// channel in Artist — matching the MPRIS tags), ellipsized to the column
/// width. `None` for the other columns (duration …), which render normally.
fn stream_column_line(
    prop: &Property<SongProperty>,
    yt: &crate::shared::ytdlp::YtStreamInfo,
    max_len: usize,
    symbols: &crate::config::theme::SymbolsConfig,
) -> Option<Line<'static>> {
    use crate::config::theme::properties::{PropertyKindOrText, SongProperty};
    let text = match &prop.kind {
        PropertyKindOrText::Property(SongProperty::Title) => yt.title.clone(),
        PropertyKindOrText::Property(SongProperty::Album) => yt.title.clone(),
        PropertyKindOrText::Property(SongProperty::Artist) => {
            yt.channel.clone().unwrap_or_default()
        }
        _ => return None,
    };
    let mut text = text;
    if text.width() > max_len {
        let mut out = String::new();
        let mut used = 0;
        let budget = max_len.saturating_sub(symbols.ellipsis.width());
        for grapheme in text.graphemes(true) {
            let w = grapheme.width();
            if used + w > budget {
                break;
            }
            out.push_str(grapheme);
            used += w;
        }
        out.push_str(&symbols.ellipsis);
        text = out;
    }
    Some(Line::from(Span::styled(text, prop.style.unwrap_or_default())))
}

/// Whether a cell holds a box's top-left corner glyph (any of the ratatui
/// border sets), used to locate the box the queue/chapters toggle sits above.
fn is_box_corner_glyph(symbol: &str) -> bool {
    matches!(symbol, "╭" | "┌" | "╒" | "╔" | "╓" | "╥")
}

#[derive(Default)]
struct QueueRow {
    cell_style: Option<Style>,
    underlined: bool,
}

impl QueueRow {
    fn into_row<'a>(self, cells: impl Iterator<Item = Line<'a>>) -> Row<'a> {
        let mut row = if let Some(style) = self.cell_style {
            Row::new(cells.map(|column| column.patch_style(style))).style(style)
        } else {
            Row::new(cells)
        };

        if self.underlined {
            row = row.style(self.cell_style.unwrap_or_default().underlined());
        }

        row
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod stream_filter_tests {
    use std::time::Duration;

    use ratatui::prelude::Rect;

    use super::{Areas, Pane, QueuePane};
    use crate::{
        ctx::Ctx,
        mpd::commands::Song,
        shared::mouse_event::{MouseEvent, MouseEventKind},
        tests::fixtures::ctx,
    };

    fn songs(n: u32) -> Vec<Song> {
        (0..n)
            .map(|i| Song {
                id: i,
                file: format!("/mnt/music/{i}.flac"),
                duration: Some(Duration::from_secs(10)),
                ..Default::default()
            })
            .collect()
    }

    fn click(pane: &mut QueuePane, row: u16, modifiers: crossterm::event::KeyModifiers, ctx: &mut Ctx) {
        let area = pane.areas[Areas::Table];
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x + 5,
                y: area.y + row,
                kind: MouseEventKind::LeftClick,
                modifiers,
            },
            ctx,
        )
        .unwrap();
    }

    fn double_click(pane: &mut QueuePane, row: u16, modifiers: crossterm::event::KeyModifiers, ctx: &mut Ctx) {
        let area = pane.areas[Areas::Table];
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x + 5,
                y: area.y + row,
                kind: MouseEventKind::DoubleClick,
                modifiers,
            },
            ctx,
        )
        .unwrap();
    }

    fn wheel(pane: &mut QueuePane, row: u16, kind: MouseEventKind, ctx: &mut Ctx) {
        let area = pane.areas[Areas::Table];
        pane.handle_mouse_event(
            MouseEvent {
                x: area.x + 5,
                y: area.y + row,
                kind,
                modifiers: crossterm::event::KeyModifiers::NONE,
            },
            ctx,
        )
        .unwrap();
    }

    #[test]
    fn plain_click_on_another_row_clears_marks() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = songs(8);
        let mut pane = QueuePane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap()).unwrap();

        // Select row 1 and mark rows 1..=3.
        pane.queue.select_idx(1, 0);
        pane.queue.state.mark_range(1, 3);
        assert_eq!(pane.queue.state.marked.len(), 3);

        // Plain click on a different row (row 4): marks are dropped.
        click(&mut pane, 4, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert!(
            pane.queue.state.marked.is_empty(),
            "a plain click on another row clears the multi-selection"
        );
        assert_eq!(pane.queue.state.get_selected(), Some(4));

        // Mark again, then click the selected row itself: marks stay.
        pane.queue.state.mark_range(1, 3);
        click(&mut pane, 4, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(
            pane.queue.state.marked.len(),
            3,
            "clicking the currently selected row keeps the marks"
        );

        // Ctrl+click keeps marking (it never clears the selection).
        click(&mut pane, 4, crossterm::event::KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(pane.queue.state.marked.len(), 4);
        assert!(pane.queue.state.marked.contains(&4));
    }

    #[test]
    fn local_queue_hides_the_temp_play_entry() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = vec![
            Song { id: 1, file: "/mnt/music/a.flac".to_owned(), duration: Some(Duration::from_secs(10)), ..Default::default() },
            Song { id: 2, file: "/mnt/music/b.flac".to_owned(), duration: Some(Duration::from_secs(20)), ..Default::default() },
        ];
        // A file played from Directories (right arrow / double-click) has a
        // temporary queue entry that must not show in the queue list.
        ctx.temp_play_id.set(Some(2));
        let local = QueuePane::local_queue(&ctx);
        assert_eq!(local.len(), 1);
        assert_eq!(local[0].id, 1);

        ctx.temp_play_id.set(None);
        assert_eq!(QueuePane::local_queue(&ctx).len(), 2);
    }

    #[rstest::rstest]
    fn local_queue_filters_out_radio_streams(mut ctx: Ctx) {
        ctx.queue = vec![
            Song { id: 1, file: "/mnt/music/a.flac".to_owned(), duration: Some(Duration::from_secs(10)), ..Default::default() },
            Song { id: 2, file: "http://stream.example/live".to_owned(), duration: None, ..Default::default() },
            Song { id: 3, file: "/mnt/music/b.flac".to_owned(), duration: Some(Duration::from_secs(20)), ..Default::default() },
        ];
        let local = QueuePane::local_queue(&ctx);
        assert_eq!(local.len(), 2);
        assert!(local.iter().all(|s| !s.file.starts_with("http")));
        assert_eq!(local[0].id, 1);
        assert_eq!(local[1].id, 3);
    }

    /// A resolved YouTube-style stream added via the paste popup's
    /// Add/Append is **queue content**: it stays visible in the queue
    /// list (radio streams and temp-play entries stay hidden).
    #[rstest::rstest]
    fn local_queue_shows_resolved_youtube_streams(mut ctx: Ctx) {
        use crate::shared::ytdlp::YtStreamInfo;
        ctx.queue = vec![
            Song { id: 1, file: "/mnt/music/a.flac".to_owned(), duration: Some(Duration::from_secs(10)), ..Default::default() },
            // A resolved YouTube stream, keyed in the yt-info cache.
            Song { id: 2, file: "https://rr4.example/audio.m4a".to_owned(), duration: None, ..Default::default() },
            // A radio station (temp entry, never cached).
            Song { id: 3, file: "http://stream.example/live".to_owned(), duration: None, ..Default::default() },
        ];
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Some Mix".to_owned(),
                ..Default::default()
            },
        );
        let local = QueuePane::local_queue(&ctx);
        let files: Vec<&str> = local.iter().map(|s| s.file.as_str()).collect();
        assert_eq!(
            files,
            vec!["/mnt/music/a.flac", "https://rr4.example/audio.m4a"],
            "the resolved stream must be visible, the radio stream hidden"
        );

        // The temp "play without adding to queue" entry is hidden too.
        ctx.temp_play_id.set(Some(2));
        assert_eq!(QueuePane::local_queue(&ctx).len(), 1);
    }

    /// The queue row of a resolved stream renders the cached info
    /// (title + channel) instead of "Unknown".
    #[test]
    fn queue_row_shows_the_cached_stream_info() {
        use crate::shared::ytdlp::YtStreamInfo;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = vec![Song {
            id: 2,
            file: "https://rr4.example/audio.m4a".to_owned(),
            duration: None,
            ..Default::default()
        }];
        ctx.yt_info.borrow_mut().insert(
            "https://rr4.example/audio.m4a".to_owned(),
            YtStreamInfo {
                url: "https://rr4.example/audio.m4a".to_owned(),
                title: "Some Mix".to_owned(),
                channel: Some("Some Channel".to_owned()),
                ..Default::default()
            },
        );
        let mut pane = QueuePane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(100, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 20), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..20u16)
            .map(|y| {
                (0..100u16).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Some Mix"), "cached title in the row: {text}");
        assert!(text.contains("Some Channel"), "cached channel in the row: {text}");
        assert!(!text.contains("Unknown"), "no Unknown placeholders: {text}");
    }

    /// A playing song with two chapters, ready for the chapters view.
    fn chapters_ctx() -> Ctx {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = vec![Song { id: 1, file: "/mnt/music/a.flac".to_owned(), ..Default::default() }];
        ctx.status.songid = Some(1);
        ctx.status.state = crate::mpd::commands::State::Play;
        ctx.chapters.borrow_mut().insert(
            "/mnt/music/a.flac".to_owned(),
            vec![
                crate::shared::chapters::Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 65.0 },
                crate::shared::chapters::Chapter { title: "Drop".into(), start_secs: 65.0, end_secs: 130.0 },
            ],
        );
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Chapters);
        ctx
    }

    #[test]
    fn chapters_first_click_highlights_double_click_seeks() {
        let mut ctx = chapters_ctx();
        let mut pane = QueuePane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();

        // A single click highlights only — even when the row is already
        // highlighted (e.g. by keyboard navigation), it never seeks.
        click(&mut pane, 1, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.chapters_state.selected(), Some(1));
        click(&mut pane, 1, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.chapters_state.selected(), Some(1), "single click never seeks");

        // A double click seeks to the chapter and drops the highlight, so
        // the next click re-highlights instead of seeking again.
        double_click(&mut pane, 1, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.chapters_state.selected(), None);

        // A different chapter gets its own highlight on its first click.
        click(&mut pane, 0, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.chapters_state.selected(), Some(0));
    }

    #[test]
    fn chapters_keyboard_navigation() {
        use std::sync::Arc;

        use crate::{
            config::keys::{CommonAction, DirectoriesActions},
            shared::keys::{ActionEvent, Actions},
        };

        let mut ctx = chapters_ctx();
        // A longer list so paging works.
        ctx.chapters.borrow_mut().clear();
        ctx.chapters.borrow_mut().insert(
            "/mnt/music/a.flac".to_owned(),
            (0..25)
                .map(|i| crate::shared::chapters::Chapter {
                    title: format!("Chapter {i}"),
                    start_secs: f64::from(i * 60),
                    end_secs: f64::from((i + 1) * 60),
                })
                .collect(),
        );
        let mut pane = QueuePane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();

        fn act(pane: &mut QueuePane, ctx: &mut Ctx, actions: Vec<Actions>) {
            let mut event = ActionEvent::from(Arc::new(actions));
            pane.handle_action(&mut event, ctx).unwrap();
        }

        // w/s (directories keys) move the highlight; the first move from no
        // selection highlights the first chapter.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.chapters_state.selected(), Some(0));
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.chapters_state.selected(), Some(1));
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderUp)]);
        assert_eq!(pane.chapters_state.selected(), Some(0));

        // ↑/↓ (common keys) move it too.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Down)]);
        assert_eq!(pane.chapters_state.selected(), Some(1));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Down)]);
        assert_eq!(pane.chapters_state.selected(), Some(2));

        // PageDown jumps by a viewport (clamped to the list end); Home/End
        // jump to the ends.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::PageDown)]);
        assert!(
            pane.chapters_state.selected().unwrap() > 2,
            "PageDown should move past the current selection"
        );
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Top)]);
        assert_eq!(pane.chapters_state.selected(), Some(0));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Bottom)]);
        assert_eq!(pane.chapters_state.selected(), Some(24));

        // Enter seeks to the highlighted chapter and keeps the highlight,
        // so the next w/s continues from the played chapter.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.chapters_state.selected(), Some(24));
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderUp)]);
        assert_eq!(pane.chapters_state.selected(), Some(23));

        // d/→ also seek from the highlight (and keep it).
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::PlayFile)]);
        assert_eq!(pane.chapters_state.selected(), Some(23));

        // The toggle key cycles back to the Audio view.
        act(
            &mut pane,
            &mut ctx,
            vec![Actions::Queue(crate::config::keys::QueueActions::ToggleChapters)],
        );
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Audio);
    }

    #[test]
    fn chapters_view_opens_on_the_current_chapter() {
        use std::sync::Arc;

        use crate::shared::keys::{ActionEvent, Actions};

        let mut ctx = chapters_ctx();
        // Playing ~70 s in: chapter 1 ("Drop") is the current chapter.
        ctx.status.elapsed = Duration::from_secs(70);
        let mut pane = QueuePane::new(&ctx);

        // Opening the queue tab with chapters mode on lands the highlight
        // on the current chapter.
        pane.before_show(&ctx).unwrap();
        assert_eq!(pane.chapters_state.selected(), Some(1));

        // Cycling the toggle away and back re-selects the current chapter
        // (the cycle is Audio -> Video -> Chapters -> Audio).
        let mut event =
            ActionEvent::from(Arc::new(vec![Actions::Queue(crate::config::keys::QueueActions::ToggleChapters)]));
        pane.handle_action(&mut event, &mut ctx).unwrap();
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Audio);
        let mut event =
            ActionEvent::from(Arc::new(vec![Actions::Queue(crate::config::keys::QueueActions::ToggleChapters)]));
        pane.handle_action(&mut event, &mut ctx).unwrap();
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Video);
        let mut event =
            ActionEvent::from(Arc::new(vec![Actions::Queue(crate::config::keys::QueueActions::ToggleChapters)]));
        pane.handle_action(&mut event, &mut ctx).unwrap();
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Chapters);
        assert_eq!(pane.chapters_state.selected(), Some(1));
    }

    #[test]
    fn toggle_renders_on_the_row_above_the_queue_box() {
        let mut ctx = chapters_ctx();
        let mut pane = QueuePane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        // A box with a top border (like the merged queue box); the toggle
        // lands on the row above it. block_area = the Queue pane's area
        // inside the box (Empty | Queue | Empty structure, so its x is two
        // cells right of the box corner).
        terminal
            .draw(|frame| {
                let block =
                    ratatui::widgets::Block::default().borders(ratatui::widgets::Borders::ALL);
                frame.render_widget(block, Rect::new(0, 1, 60, 8));
                pane.render_toggle_on_border(
                    frame,
                    ratatui::widgets::Borders::NONE,
                    Rect::new(2, 4, 55, 4),
                    &ctx,
                );
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The toggle sits on its own row above the box, one cell in from
        // the corner: ` ● Audio ⭘ Video ⭘ Chapters`.
        let line: String = (0..60).map(|x| buf[(x, 0)].symbol().to_string()).collect();
        assert!(
            line.contains("Audio") && line.contains("Video") && line.contains("Chapters"),
            "toggle missing from the row above the box: {line}"
        );
        assert_eq!(buf[(1, 0)].symbol(), " ");
        // The box top border below is untouched.
        assert!(matches!(buf[(0, 1)].symbol(), "╭" | "┌"));
        assert!(matches!(buf[(59, 1)].symbol(), "╮" | "┐"));
        // The clickable segments point at the toggle row.
        assert_eq!(pane.toggle_areas[0].y, 0);
        assert_eq!(pane.toggle_areas[1].y, 0);

        // A box without a top border: no corner found, no toggle.
        pane.toggle_areas = [Rect::default(); 3];
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 40)).unwrap();
        terminal
            .draw(|frame| {
                let block = ratatui::widgets::Block::default()
                    .borders(ratatui::widgets::Borders::LEFT
                        | ratatui::widgets::Borders::RIGHT
                        | ratatui::widgets::Borders::BOTTOM);
                frame.render_widget(block, Rect::new(0, 1, 60, 8));
                pane.render_toggle_on_border(
                    frame,
                    ratatui::widgets::Borders::NONE,
                    Rect::new(2, 4, 55, 4),
                    &ctx,
                );
            })
            .unwrap();
        assert_eq!(pane.toggle_areas[0], Rect::default());
    }

    #[test]
    fn chapters_values_line_up_under_the_time_and_duration_headers() {
        let mut ctx = chapters_ctx();
        // The header renders into the width stashed by the queue pane, so
        // its labels sit exactly above the chapter values.
        ctx.queue_table_width.set(Some(60));
        let mut queue_pane = QueuePane::new(&ctx);
        let mut header_pane = crate::ui::panes::queue_header::QueueHeaderPane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(70, 20);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| {
                header_pane.render(frame, Rect::new(0, 0, 62, 1), &ctx).unwrap();
                queue_pane.render(frame, Rect::new(0, 1, 62, 19), &ctx).unwrap();
            })
            .unwrap();
        let buf = terminal.backend().buffer();

        // The first chapter row shows start "0:00" and duration "1:05".
        // Time is centered under the Time label (both are 4 chars wide, so
        // they share a start column); Duration is right-aligned, so the
        // value ends in the same column as the label.
        assert_eq!(buf[(0, 0)].symbol(), "C"); // Chapter
        assert_eq!(buf[(0, 1)].symbol(), "❯"); // current chapter marker
        let time_x = find_text(&buf, 0, "Time").unwrap_or_else(|| {
            panic!("Time label not found in the chapters header")
        });
        assert_eq!(
            buf[(time_x, 1)].symbol(),
            "0",
            "time does not start under the Time header"
        );
        let dur_x = find_text(&buf, 0, "Duration").unwrap_or_else(|| {
            panic!("Duration label not found in the chapters header")
        });
        let dur_right = dur_x + "Duration".len() as u16;
        assert_eq!(
            buf[(dur_right - 1, 1)].symbol(),
            "5",
            "duration does not end under the Duration header"
        );
    }

    /// Left-most x of a text run on row `y` of the buffer.
    fn find_text(buf: &ratatui::buffer::Buffer, y: u16, text: &str) -> Option<u16> {
        let cells: Vec<String> = (0..buf.area.width).map(|x| buf[(x, y)].symbol().to_string()).collect();
        (0..cells.len())
            .find(|x| {
                cells[*x..].starts_with(&text.chars().map(|c| c.to_string()).collect::<Vec<_>>())
            })
            .map(|x| x as u16)
    }

    /// A ctx with the persistent video playlist holding a 3-entry list and
    /// an mpv session playing the same list, currently at entry 1.
    fn video_ctx() -> Ctx {
        use crate::core::mpv::{MpvPlaylistEntry, MpvSession};
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let entries = vec![
            MpvPlaylistEntry::new(
                "Pilot",
                "http://jf/Videos/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/stream",
                Some(2400.0),
            ),
            MpvPlaylistEntry::new(
                "Second",
                "http://jf/Videos/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb/stream",
                Some(2500.0),
            ),
            MpvPlaylistEntry::new(
                "Finale",
                "http://jf/Videos/cccccccccccccccccccccccccccccccc/stream",
                Some(2600.0),
            ),
        ];
        *ctx.video_playlist.borrow_mut() = entries.clone();
        ctx.mpv = MpvSession {
            active: true,
            socket: Some(std::path::PathBuf::from("/tmp/fake.sock")),
            playlist: std::cell::RefCell::new(entries),
            playlist_pos: std::cell::Cell::new(Some(1)),
            ..Default::default()
        };
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        ctx
    }

    #[test]
    fn video_tab_lists_the_mpv_playlist_and_marks_the_current_entry() {
        let mut ctx = video_ctx();
        let mut pane = QueuePane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let rows: Vec<String> = (0..40u16)
            .map(|y| (0..100).map(|x| buf[(x, y)].symbol().to_string()).collect::<String>())
            .collect();
        let body = rows.join("\n");
        assert!(body.contains("Pilot"), "playlist entry missing: {body}");
        assert!(body.contains("Second"));
        assert!(body.contains("Finale"));
        // Durations render right-aligned in the Duration column.
        assert!(body.contains("40:00"), "duration 40:00 missing: {body}");
        // The current entry gets the ❯ marker.
        let current_row = rows.iter().position(|r| r.contains("Second")).unwrap();
        assert!(
            rows[current_row].contains("❯"),
            "current entry not marked: {}",
            rows[current_row]
        );
    }

    #[test]
    fn video_tab_click_highlights_then_loads_and_keyboard_navigates() {
        use std::sync::Arc;

        use crate::{
            config::keys::{CommonAction, DirectoriesActions},
            shared::keys::{ActionEvent, Actions},
        };

        let mut ctx = video_ctx();
        let mut pane = QueuePane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();

        // A single click highlights only — even on the currently playing
        // entry, which the list opens with already highlighted: the first
        // click must never reload the stream.
        pane.before_show(&ctx).unwrap();
        assert_eq!(
            pane.video_state.selected(),
            Some(1),
            "the playing entry is highlighted when the list opens"
        );
        click(&mut pane, 1, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(
            pane.video_state.selected(),
            Some(1),
            "a single click on the playing entry only keeps the highlight"
        );
        // Clicking a different entry moves the highlight (still no load).
        click(&mut pane, 0, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.video_state.selected(), Some(0));
        // A double click loads the highlighted entry and drops the
        // highlight (mpv IPC — the fake socket just fails silently).
        double_click(&mut pane, 0, crossterm::event::KeyModifiers::NONE, &mut ctx);
        assert_eq!(pane.video_state.selected(), None);

        fn act(pane: &mut QueuePane, ctx: &mut Ctx, actions: Vec<Actions>) {
            let mut event = ActionEvent::from(Arc::new(actions));
            pane.handle_action(&mut event, ctx).unwrap();
        }

        // w/s move the highlight; the first move selects the first entry.
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.video_state.selected(), Some(0));
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::FolderDown)]);
        assert_eq!(pane.video_state.selected(), Some(1));
        // → loads the highlighted entry (no panic on the fake socket);
        // Enter opens the context menu instead (the highlight stays).
        act(&mut pane, &mut ctx, vec![Actions::Directories(DirectoriesActions::PlayFile)]);
        assert_eq!(pane.video_state.selected(), Some(1));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Confirm)]);
        assert_eq!(pane.video_state.selected(), Some(1));
        // PageDown / Home / End navigate.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Top)]);
        assert_eq!(pane.video_state.selected(), Some(0));
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::Bottom)]);
        assert_eq!(pane.video_state.selected(), Some(2));
        // `c` cycles back to Audio (no chapters -> Chapters is skipped).
        act(
            &mut pane,
            &mut ctx,
            vec![Actions::Queue(crate::config::keys::QueueActions::ToggleChapters)],
        );
        assert_eq!(ctx.queue_tab.get(), crate::ctx::QueueTabMode::Audio);
    }

    /// The wheel moves the audio list's highlight (like w/s) and clamps at
    /// both ends — it never leaves the highlight stuck below the top like
    /// the old viewport-only scroll did.
    #[test]
    fn wheel_moves_the_audio_list_highlight() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue = songs(8);
        let mut pane = QueuePane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();

        assert_eq!(pane.queue.state.get_selected(), Some(0), "the list opens on the first row");
        wheel(&mut pane, 2, MouseEventKind::ScrollDown, &mut ctx);
        assert_eq!(pane.queue.state.get_selected(), Some(1), "wheel down moves the highlight");
        wheel(&mut pane, 2, MouseEventKind::ScrollDown, &mut ctx);
        assert_eq!(pane.queue.state.get_selected(), Some(2));
        wheel(&mut pane, 2, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.queue.state.get_selected(), Some(1), "wheel up moves it back");
        wheel(&mut pane, 2, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.queue.state.get_selected(), Some(0), "wheel up reaches the top");
        wheel(&mut pane, 2, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.queue.state.get_selected(), Some(0), "wheel up clamps at the top");
        for _ in 0..20 {
            wheel(&mut pane, 2, MouseEventKind::ScrollDown, &mut ctx);
        }
        assert_eq!(pane.queue.state.get_selected(), Some(7), "wheel down clamps at the last row");
    }

    /// The wheel moves the video list's highlight (like w/s) and clamps at
    /// both ends.
    #[test]
    fn wheel_moves_the_video_list_highlight() {
        let mut ctx = video_ctx();
        let mut pane = QueuePane::new(&ctx);

        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();
        pane.before_show(&ctx).unwrap();

        assert_eq!(pane.video_state.selected(), Some(1), "the playing entry is highlighted");
        wheel(&mut pane, 1, MouseEventKind::ScrollDown, &mut ctx);
        assert_eq!(pane.video_state.selected(), Some(2), "wheel down moves the highlight");
        wheel(&mut pane, 1, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.video_state.selected(), Some(1), "wheel up moves it back");
        wheel(&mut pane, 1, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.video_state.selected(), Some(0), "wheel up reaches the top");
        wheel(&mut pane, 1, MouseEventKind::ScrollUp, &mut ctx);
        assert_eq!(pane.video_state.selected(), Some(0), "wheel up clamps at the top");
        for _ in 0..10 {
            wheel(&mut pane, 1, MouseEventKind::ScrollDown, &mut ctx);
        }
        assert_eq!(pane.video_state.selected(), Some(2), "wheel down clamps at the last row");
    }

    #[test]
    fn switching_to_a_youtube_video_reloads_info_chapters_and_art() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        use crate::core::mpv::{MpvPlaylistEntry, MpvSession};
        use crate::shared::chapters::Chapter;
        use crate::shared::ytdlp::YtStreamInfo;

        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (work_tx, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        // Two resolved YouTube streams: info + chapters keyed by the
        // resolved URL and the original link (as apply_resolved_streams
        // does).
        let noita_url = "https://www.youtube.com/watch?v=fUorqpw7UJM";
        let lav_url = "https://www.youtube.com/watch?v=Hc9qrvQ3QPg";
        let noita = YtStreamInfo {
            url: "https://rr.googlevideo.com/noita".to_owned(),
            original_url: noita_url.to_owned(),
            title: "This New Noita-Like Is Surprisingly Good".to_owned(),
            thumbnail: Some("https://i.ytimg.com/vi/fUorqpw7UJM/maxresdefault.jpg".to_owned()),
            description: Some("noita description https://example.com".to_owned()),
            subscribers: None,
            ..Default::default()
        };
        let lav = YtStreamInfo {
            url: "https://rr.googlevideo.com/lav".to_owned(),
            original_url: lav_url.to_owned(),
            title: "I tested EVERY 32bit float wireless lav mic".to_owned(),
            channel: Some("Some Channel".to_owned()),
            thumbnail: Some("https://i.ytimg.com/vi/Hc9qrvQ3QPg/maxresdefault.jpg".to_owned()),
            description: Some("lav description".to_owned()),
            subscribers: None,
            duration: Some(900.0),
            chapters: vec![
                Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 },
                Chapter { title: "Main".into(), start_secs: 60.0, end_secs: 900.0 },
            ],
        };
        for item in [&noita, &lav] {
            ctx.yt_info.borrow_mut().insert(item.url.clone(), item.clone());
            ctx.yt_info.borrow_mut().insert(item.original_url.clone(), item.clone());
            ctx.chapters.borrow_mut().insert(item.url.clone(), item.chapters.clone());
            ctx.chapters.borrow_mut().insert(item.original_url.clone(), item.chapters.clone());
        }
        // The session plays the Noita video; the persistent video playlist
        // holds both videos.
        *ctx.video_playlist.borrow_mut() = vec![
            MpvPlaylistEntry::new(noita.title.clone(), noita_url.to_owned(), None),
            MpvPlaylistEntry::new(lav.title.clone(), lav_url.to_owned(), None),
        ];
        // A live fake mpv socket so the running-instance switch path is
        // taken (a dead socket would launch a fresh mpv instead).
        let dir = std::env::temp_dir().join(format!("yt-switch-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock_path = dir.join("mpv.sock");
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let _ = writeln!(reader.get_mut(), r#"{{"error":"success","data":0}}"#);
                    let _ = reader.get_mut().flush();
                    line.clear();
                }
            }
        });
        ctx.mpv = MpvSession {
            active: true,
            socket: Some(sock_path),
            playlist: std::cell::RefCell::new(vec![MpvPlaylistEntry::new(
                noita.title.clone(),
                noita_url.to_owned(),
                None,
            )]),
            playlist_pos: std::cell::Cell::new(Some(0)),
            ..Default::default()
        };
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);

        // Switch to the lav-mic video (index 1 of the persistent list).
        let pane = QueuePane::new(&ctx);
        pane.video_load_entry(1, &ctx);

        // The session playlist now points at the new video…
        assert_eq!(ctx.mpv.playlist_pos.get(), Some(0));
        assert_eq!(ctx.mpv.playlist.borrow().len(), 1);
        assert_eq!(ctx.mpv.playlist.borrow()[0].url, lav_url);
        // …and the info box / chapters / album art all resolve the new
        // video's data through it (the MpvItemChanged handler reads the
        // same lookups).
        let info = crate::ui::modals::paste::mpv_yt_info(&ctx).expect("yt info of the new video");
        assert_eq!(info.title, lav.title);
        assert_eq!(info.thumbnail.as_deref(), lav.thumbnail.as_deref());
        assert_eq!(ctx.current_playback_chapters().len(), 2, "chapters reloaded");
        assert_eq!(ctx.current_playback_chapters()[1].title, "Main");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn video_tab_shows_a_hint_when_nothing_plays() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        let mut pane = QueuePane::new(&ctx);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let body: String = (0..40u16)
            .flat_map(|y| (0..100).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(body.contains("No video playing"), "hint missing: {body}");
    }

    #[test]
    fn loading_a_video_from_the_list_switches_the_session() {
        use std::io::{BufRead, BufReader, Write};
        use std::os::unix::net::UnixListener;

        use crate::shared::events::AppEvent;
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = video_ctx();
        // Rebuild the fixture with a live app-event receiver.
        let (work_tx, _work_rx) = crossbeam::channel::unbounded();
        let mut fresh = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (work_tx, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        fresh.mpv = ctx.mpv.clone();
        *fresh.video_playlist.borrow_mut() = ctx.video_playlist.borrow().clone();
        fresh.queue_tab.set(crate::ctx::QueueTabMode::Video);

        // A live fake mpv socket, so the session-switch path is taken (a
        // dead socket would launch a fresh mpv instead).
        let dir = std::env::temp_dir().join(format!("video-load-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let sock_path = dir.join("mpv.sock");
        fresh.mpv.socket = Some(sock_path.clone());
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path).unwrap();
        std::thread::spawn(move || {
            if let Ok((stream, _)) = listener.accept() {
                let mut reader = BufReader::new(stream);
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) {
                    let _ = writeln!(reader.get_mut(), r#"{{"error":"success","data":0}}"#);
                    let _ = reader.get_mut().flush();
                    line.clear();
                }
            }
        });

        let pane = QueuePane::new(&fresh);
        // Load entry 2 ("Finale", a Jellyfin URL): the session plays the
        // playlist from that entry onwards.
        pane.video_load_entry(2, &fresh);

        // The session playlist is the [2..] slice, starting at position 0
        // (the persistent playlist itself is untouched).
        assert_eq!(fresh.mpv.playlist_pos.get(), Some(0));
        assert_eq!(fresh.mpv.playlist.borrow().len(), 1);
        assert_eq!(fresh.mpv.playlist.borrow()[0].title, "Finale");
        assert_eq!(fresh.video_playlist.borrow().len(), 3, "persistent list untouched");
        // The event switches the session title/item and fetches the new
        // item's metadata/chapters/art.
        let event = app_rx.try_recv().expect("MpvItemChanged event");
        assert!(matches!(
            event,
            AppEvent::UiEvent(crate::ui::UiAppEvent::MpvItemChanged { item_id, title })
                if title == "Finale"
                    && item_id == "cccccccccccccccccccccccccccccccc"
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let _ = ctx;
    }

    #[test]
    fn video_chapters_come_from_the_playing_mpv_item() {
        use crate::shared::chapters::Chapter;
        let mut ctx = video_ctx();
        // The playing Jellyfin video has chapter markers, keyed by its item
        // id (there is no queue song for a video).
        let item_id = "abcdef0123456789abcdef0123456789";
        ctx.mpv.item_id = Some(item_id.to_owned());
        ctx.chapters.borrow_mut().insert(
            item_id.to_owned(),
            vec![
                Chapter { title: "Cold Open".into(), start_secs: 0.0, end_secs: 300.0 },
                Chapter { title: "Main Title".into(), start_secs: 300.0, end_secs: 900.0 },
            ],
        );
        assert!(QueuePane::chapters_available(&ctx));
        assert_eq!(QueuePane::current_chapters(&ctx).len(), 2);

        let mut pane = QueuePane::new(&ctx);
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Chapters);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), &ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let body: String = (0..40u16)
            .flat_map(|y| (0..100).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(
            body.contains("Cold Open") && body.contains("Main Title"),
            "video chapters missing from the Chapters view: {body}"
        );
        // Without the item's chapters the Chapters tab is not offered.
        ctx.chapters.borrow_mut().remove(item_id);
        assert!(!QueuePane::chapters_available(&ctx));
    }

    #[test]
    fn video_view_swaps_to_the_jellyfin_playlist_and_back() {
        use crate::core::mpv::{MpvPlaylistEntry, MpvSession};
        // The persistent queue holds non-Jellyfin entries; the session
        // plays a Jellyfin season.
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        *ctx.video_playlist.borrow_mut() = vec![
            MpvPlaylistEntry::new("My video A", "https://yt/a", None),
            MpvPlaylistEntry::new("My video B", "https://yt/b", None),
        ];
        ctx.mpv = MpvSession {
            active: true,
            item_id: Some("0123456789abcdef0123456789abcdef".to_owned()),
            socket: Some(std::path::PathBuf::from("/tmp/fake.sock")),
            playlist: std::cell::RefCell::new(vec![
                MpvPlaylistEntry::new(
                    "Episode One",
                    "http://jf/Videos/0123456789abcdef0123456789abcdef/stream",
                    None,
                ),
                MpvPlaylistEntry::new(
                    "Episode Two",
                    "http://jf/Videos/0123456789abcdef0123456789abcdef/stream?x=2",
                    None,
                ),
            ]),
            playlist_pos: std::cell::Cell::new(Some(1)),
            ..Default::default()
        };
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        let mut pane = QueuePane::new(&ctx);
        let render_body = |pane: &mut QueuePane, ctx: &Ctx| {
            let backend = ratatui::backend::TestBackend::new(100, 40);
            let mut terminal = ratatui::Terminal::new(backend).unwrap();
            terminal
                .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), ctx).unwrap())
                .unwrap();
            let buf = terminal.backend().buffer();
            (0..40u16)
                .flat_map(|y| (0..100).map(move |x| buf[(x, y)].symbol().to_string()))
                .collect::<String>()
        };

        // Jellyfin playing: the season is shown, the queue is cached away.
        assert!(crate::core::mpv::session_playlist_shown(&ctx));
        let body = render_body(&mut pane, &ctx);
        assert!(body.contains("Episode One") && body.contains("Episode Two"), "{body}");
        assert!(!body.contains("My video A"), "queue must be hidden: {body}");
        assert_eq!(pane.video_items_len, 2);
        // Delete is disabled while the Jellyfin playlist is shown: the
        // persistent queue is untouched.
        let before = ctx.video_playlist.borrow().len();
        pane.video_state.select(Some(1));
        let mut event = crate::ui::ActionEvent::from(std::sync::Arc::new(vec![
            crate::ui::Actions::Common(crate::config::keys::CommonAction::Delete),
        ]));
        pane.handle_action(&mut event, &mut ctx).unwrap();
        assert_eq!(ctx.video_playlist.borrow().len(), before, "queue untouched");

        // Jellyfin playback stops: the persistent queue is restored.
        ctx.mpv = MpvSession::default();
        assert!(!crate::core::mpv::session_playlist_shown(&ctx));
        let body = render_body(&mut pane, &ctx);
        assert!(body.contains("My video A") && body.contains("My video B"), "{body}");
        assert!(!body.contains("Episode One"), "{body}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod follow_video_tests {
    use crate::{
        ctx::{Ctx, QueueTabMode},
        mpd::commands::State,
        shared::{chapters::Chapter, events::WorkRequest},
        tests::fixtures::ctx,
        ui::panes::{
            Pane,
            QueuePane,
            queue::{play_queue_song, resolved_stream_expired},
        },
    };

    fn queue_ctx() -> (Ctx, crossbeam::channel::Sender<crate::shared::events::AppEvent>) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = ctx(
            (app_tx.clone(), _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_tx)
    }

    fn with_video_chapters(ctx: &mut Ctx, item_id: &str) {
        ctx.mpv.active = true;
        ctx.mpv.item_id = Some(item_id.to_owned());
        ctx.chapters.borrow_mut().insert(
            item_id.to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
    }

    #[test]
    fn video_with_chapters_switches_to_the_chapters_list() {
        let (mut ctx, _tx) = queue_ctx();
        with_video_chapters(&mut ctx, "0123456789abcdef0123456789abcdef");
        ctx.queue_tab.set(QueueTabMode::Audio);
        let mut pane = QueuePane::new(&ctx);

        pane.follow_playing_video(&ctx);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Chapters);
    }

    #[test]
    fn video_without_chapters_switches_to_the_video_list() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.mpv.active = true;
        ctx.mpv.item_id = Some("0123456789abcdef0123456789abcdef".to_owned());
        ctx.queue_tab.set(QueueTabMode::Audio);
        let mut pane = QueuePane::new(&ctx);

        pane.follow_playing_video(&ctx);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Video);
    }

    #[test]
    fn auto_chapters_off_lands_on_the_video_list_even_with_chapters() {
        let (mut ctx, _tx) = queue_ctx();
        with_video_chapters(&mut ctx, "0123456789abcdef0123456789abcdef");
        let mut config = ctx.config.as_ref().clone();
        config.ui.auto_show_chapters = false;
        ctx.config = std::sync::Arc::new(config);
        ctx.queue_tab.set(QueueTabMode::Audio);
        let mut pane = QueuePane::new(&ctx);

        pane.follow_playing_video(&ctx);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Video);
    }

    #[test]
    fn no_mpv_session_leaves_the_mode_untouched() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.queue_tab.set(QueueTabMode::Audio);
        let mut pane = QueuePane::new(&ctx);

        pane.follow_playing_video(&ctx);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Audio);
    }

    /// A chaptered audio track that starts playing (its chapters arrive
    /// after the song change) auto-opens the Queue tab's Chapters list;
    /// the active tab is never switched.
    #[test]
    fn chaptered_audio_track_auto_opens_the_chapters_list() {
        let (mut ctx, _tx) = queue_ctx();
        // The current song just gained markers (ffprobe / jellyfin / yt).
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        ctx.queue = vec![crate::mpd::commands::Song {
            id: 1,
            file: "audio/song.flac".to_owned(),
            ..Default::default()
        }];
        ctx.chapters.borrow_mut().insert(
            "audio/song.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        ctx.queue_tab.set(QueueTabMode::Audio);

        ctx.auto_show_chapters();
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Chapters);
    }

    #[test]
    fn auto_chapters_off_leaves_the_list_untouched() {
        let (mut ctx, _tx) = queue_ctx();
        let mut config = ctx.config.as_ref().clone();
        config.ui.auto_show_chapters = false;
        ctx.config = std::sync::Arc::new(config);
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        ctx.queue = vec![crate::mpd::commands::Song {
            id: 1,
            file: "audio/song.flac".to_owned(),
            ..Default::default()
        }];
        ctx.chapters.borrow_mut().insert(
            "audio/song.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        ctx.queue_tab.set(QueueTabMode::Audio);

        ctx.auto_show_chapters();
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Audio);
    }

    /// While a video plays in mpv the Queue list is owned by
    /// `follow_playing_video`; the audio auto-chapters switch must not
    /// fight it.
    #[test]
    fn auto_chapters_ignored_while_a_video_plays() {
        let (mut ctx, _tx) = queue_ctx();
        with_video_chapters(&mut ctx, "0123456789abcdef0123456789abcdef");
        // The paused audio song has chapters too.
        ctx.status.state = State::Pause;
        ctx.status.songid = Some(1);
        ctx.queue = vec![crate::mpd::commands::Song {
            id: 1,
            file: "audio/song.flac".to_owned(),
            ..Default::default()
        }];
        ctx.chapters.borrow_mut().insert(
            "audio/song.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        ctx.queue_tab.set(QueueTabMode::Video);

        ctx.auto_show_chapters();
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Video);
    }

    /// MPD playback taking over (the mutual exclusion paused the video)
    /// makes the music the UI source: the Queue list no longer follows the
    /// paused video, and a chaptered music track auto-opens its Chapters
    /// list instead.
    #[test]
    fn mpd_takeover_moves_chapters_to_the_music() {
        let (mut ctx, _tx) = queue_ctx();
        with_video_chapters(&mut ctx, "0123456789abcdef0123456789abcdef");
        ctx.mpv.paused = true;
        ctx.status.state = State::Play;
        ctx.status.songid = Some(1);
        ctx.queue = vec![crate::mpd::commands::Song {
            id: 1,
            file: "audio/song.flac".to_owned(),
            ..Default::default()
        }];
        ctx.chapters.borrow_mut().insert(
            "audio/song.flac".to_owned(),
            vec![Chapter { title: "Intro".into(), start_secs: 0.0, end_secs: 60.0 }],
        );
        // The chapters list is showing the video's markers.
        ctx.queue_tab.set(QueueTabMode::Chapters);
        let mut pane = QueuePane::new(&ctx);

        // The video's chapters no longer apply; the song's do.
        assert_eq!(ctx.current_playback_chapters()[0].title, "Intro");
        // Re-following the video session (e.g. late-arriving JF chapters)
        // must not yank the list back to the paused video.
        pane.follow_playing_video(&ctx);
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Chapters);
        // From the Audio list, the chaptered music auto-opens its own
        // Chapters list.
        ctx.queue_tab.set(QueueTabMode::Audio);
        ctx.auto_show_chapters();
        assert_eq!(ctx.queue_tab.get(), QueueTabMode::Chapters);
    }

    #[test]
    fn resolved_stream_expiry_detection() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let future = now as i64 + 86_400;
        // An expired googlevideo stream URL is flagged.
        assert!(resolved_stream_expired(
            "https://rr4.googlevideo.com/videoplayback?expire=1000&x=1"
        ));
        // A live one is not.
        assert!(!resolved_stream_expired(&format!(
            "https://rr.googlevideo.com/videoplayback?expire={future}"
        )));
        // Non-stream URLs (local files, original links) never count.
        assert!(!resolved_stream_expired("/mnt/music/track.flac"));
        assert!(!resolved_stream_expired("https://youtu.be/185XGEMefgc"));
        assert!(!resolved_stream_expired("https://soundcloud.com/foo/bar"));
    }

    /// A queue entry whose signed stream URL expired can't play as-is:
    /// playing it re-resolves the original link and replaces the dead
    /// entry instead of sending the stale URL to MPD.
    #[test]
    fn playing_an_expired_stream_reresolves_from_the_original_link() {
        use crate::shared::ytdlp::YtStreamInfo;
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        let mut ctx =
            ctx((app_tx, _app_rx), (work_tx, work_rx.clone()), (client_tx, client_rx.clone()));
        let expired = "https://rr4.googlevideo.com/videoplayback?expire=1000&id=abc";
        let original = "https://www.youtube.com/watch?v=abc123";
        let song = crate::mpd::commands::Song {
            id: 1,
            file: expired.to_owned(),
            ..Default::default()
        };
        ctx.queue = vec![song.clone()];
        // The cached info still knows the original link of the dead stream.
        ctx.yt_info.borrow_mut().insert(
            expired.to_owned(),
            YtStreamInfo {
                url: expired.to_owned(),
                original_url: original.to_owned(),
                ..Default::default()
            },
        );

        play_queue_song(&song, &ctx);
        // A re-resolution of the original link is requested…
        match work_rx.try_recv() {
            Ok(WorkRequest::ResolveYtStreams { urls, action }) => {
                assert_eq!(urls, vec![original.to_owned()]);
                assert!(matches!(action, crate::ui::modals::paste::YtAction::ReplaceAndPlay(1)));
            }
            _ => panic!("expected a ResolveYtStreams request for the original link"),
        }
        // …and no play command is sent for the dead URL.
        assert!(
            client_rx.try_recv().is_err(),
            "no MPD command should be queued for the expired URL"
        );
    }

    /// Without the original link the expired entry can't be re-resolved:
    /// the play is still attempted (the failure is explained by the status
    /// line) and nothing is sent to the work thread.
    #[test]
    fn playing_an_expired_stream_without_cached_info_plays_as_is() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let (work_tx, work_rx) = crossbeam::channel::unbounded();
        let (client_tx, client_rx) = crossbeam::channel::unbounded();
        let mut ctx =
            ctx((app_tx, _app_rx), (work_tx, work_rx.clone()), (client_tx, client_rx.clone()));
        let expired = "https://rr4.googlevideo.com/videoplayback?expire=1000&id=def";
        let song = crate::mpd::commands::Song {
            id: 2,
            file: expired.to_owned(),
            ..Default::default()
        };
        ctx.queue = vec![song.clone()];

        play_queue_song(&song, &ctx);
        assert!(work_rx.try_recv().is_err(), "no re-resolution without the original link");
        assert!(
            matches!(client_rx.try_recv(), Ok(_)),
            "the play command still goes to MPD"
        );
    }
}

/// The Video list supports the audio queue's multi-selection: ctrl+click
/// toggles a mark, alt+click ranges from the anchor, Shift+Up/Down
/// range-selects (each press replaces the previous range) and Del removes
/// every marked entry.
#[cfg(test)]
mod video_mark_tests {
    use super::*;
    use crate::{
        shared::{
            keys::{ActionEvent, Actions},
            mouse_event::{MouseEvent, MouseEventKind},
        },
        tests::fixtures::ctx,
        ui::panes::Pane,
    };
    use crossterm::event::KeyModifiers;
    use ratatui::prelude::Rect;

    fn queue_ctx() -> (Ctx, crossbeam::channel::Sender<crate::shared::events::AppEvent>) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = ctx(
            (app_tx.clone(), _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_tx)
    }

    fn entries(n: usize) -> Vec<crate::core::mpv::MpvPlaylistEntry> {
        (0..n)
            .map(|i| crate::core::mpv::MpvPlaylistEntry::new(format!("v{i}"), format!("url{i}"), None))
            .collect()
    }

    fn action(actions: Vec<Actions>) -> ActionEvent {
        ActionEvent::from(std::sync::Arc::new(actions))
    }

    fn video_pane(ctx: &mut Ctx) -> QueuePane {
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        let mut pane = QueuePane::new(ctx);
        pane.video_items_len = ctx.video_playlist.borrow().len();
        pane
    }

    fn render(pane: &mut QueuePane, ctx: &Ctx) -> Rect {
        let backend = ratatui::backend::TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        pane.areas[Areas::Table]
    }

    #[test]
    fn shift_down_and_up_range_selects() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);

        // Move the cursor to row 2 first (plain moves select only).
        for _ in 0..3 {
            let mut ev = action(vec![Actions::Common(CommonAction::Down)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        assert_eq!(pane.video_state.selected(), Some(2));
        assert!(pane.video_marked.is_empty());

        // Shift+Down from row 2 marks 2..=3; the next press extends to 4.
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3]);
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3, 4]);

        // Shift+Up contracts the range, unmarking the row left behind.
        let mut ev = action(vec![Actions::Common(CommonAction::SelectUp)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3]);
        assert_eq!(pane.video_state.selected(), Some(3));
    }

    #[test]
    fn shift_reanchors_after_the_video_marks_are_cleared() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(8));
        let mut pane = video_pane(&mut ctx);

        // Move the cursor to row 2, then Shift+Down marks [2..=3] (anchor 2).
        for _ in 0..3 {
            let mut ev = action(vec![Actions::Common(CommonAction::Down)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3]);

        // Selection cleared, cursor moved to row 6: the next Shift+Down
        // starts a fresh range at the cursor instead of the old anchor.
        pane.video_marked.clear();
        for _ in 0..3 {
            let mut ev = action(vec![Actions::Common(CommonAction::Down)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        assert_eq!(pane.video_state.selected(), Some(6));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_state.selected(), Some(7));
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![6, 7]);
    }

    #[test]
    fn ctrl_click_toggles_and_plain_click_clears() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        // ctrl+click rows 2 and 4: both marked.
        for row in [2u16, 4] {
            pane.handle_mouse_event(
                MouseEvent {
                    x: table.x + 1,
                    y: table.y + row,
                    kind: MouseEventKind::LeftClick,
                    modifiers: KeyModifiers::CONTROL,
                },
                &ctx,
            )
            .unwrap();
        }
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 4]);
        assert_eq!(pane.video_state.selected(), Some(4));

        // ctrl+click row 2 again: toggled off.
        pane.handle_mouse_event(
            MouseEvent {
                x: table.x + 1,
                y: table.y + 2,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::CONTROL,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![4]);

        // A plain click on a different row clears the marks.
        pane.handle_mouse_event(
            MouseEvent {
                x: table.x + 1,
                y: table.y + 1,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::NONE,
            },
            &ctx,
        )
        .unwrap();
        assert!(pane.video_marked.is_empty());
        assert_eq!(pane.video_state.selected(), Some(1));
    }

    #[test]
    fn alt_click_ranges_from_the_anchor() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        // Plain-click row 1 sets the anchor; alt+click row 4 marks 1..=4.
        for (row, modifiers) in [(1u16, KeyModifiers::NONE), (4, KeyModifiers::ALT)] {
            pane.handle_mouse_event(
                MouseEvent {
                    x: table.x + 1,
                    y: table.y + row,
                    kind: MouseEventKind::LeftClick,
                    modifiers,
                },
                &ctx,
            )
            .unwrap();
        }
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![1, 2, 3, 4]);

        // alt+click closer to the anchor (row 2) contracts the range.
        pane.handle_mouse_event(
            MouseEvent {
                x: table.x + 1,
                y: table.y + 2,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::ALT,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![1, 2]);
    }

    #[test]
    fn alt_click_reanchors_after_the_marks_are_cleared() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        // Mark rows 1..=4 via plain-click anchor + alt+click.
        for (row, modifiers) in [(1u16, KeyModifiers::NONE), (4, KeyModifiers::ALT)] {
            pane.handle_mouse_event(
                MouseEvent {
                    x: table.x + 1,
                    y: table.y + row,
                    kind: MouseEventKind::LeftClick,
                    modifiers,
                },
                &ctx,
            )
            .unwrap();
        }
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![1, 2, 3, 4]);

        // Esc drops the marks AND the anchor: the next alt+click starts a
        // fresh anchor at the clicked row (nothing is range-marked yet),
        // and the following one ranges from it - never back to the stale
        // anchor (1).
        pane.video_marked.clear();
        pane.video_marked.clear_anchor();
        pane.video_state.select(Some(5));
        pane.handle_mouse_event(
            MouseEvent {
                x: table.x + 1,
                y: table.y + 5,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::ALT,
            },
            &ctx,
        )
        .unwrap();
        assert!(
            pane.video_marked.is_empty(),
            "the first alt+click after Esc sets the fresh anchor only"
        );

        // A second alt+click ranges from the fresh anchor.
        pane.handle_mouse_event(
            MouseEvent {
                x: table.x + 1,
                y: table.y + 2,
                kind: MouseEventKind::LeftClick,
                modifiers: KeyModifiers::ALT,
            },
            &ctx,
        )
        .unwrap();
        assert_eq!(
            pane.video_marked.iter().collect::<Vec<_>>(),
            vec![2, 3, 4, 5],
            "the range spans the fresh anchor, not the stale one"
        );
    }

    #[test]
    fn delete_removes_every_marked_entry() {
        let (mut ctx, _tx) = queue_ctx();
        // Redirect the persisted video playlist to a temp dir so the test
        // never touches the real cache.
        let tag = format!("queue-video-mark-{}", std::process::id());
        let tmp = std::env::temp_dir().join(tag);
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let mut config = ctx.config.as_ref().clone();
        config.cache_dir = Some(tmp.clone());
        ctx.config = std::sync::Arc::new(config);
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);

        // Mark rows 2..=4 via Shift+Down from row 2.
        pane.video_state.select(Some(2));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3, 4]);

        // Del removes the marked entries; the rest stay in order.
        let mut ev = action(vec![Actions::Common(CommonAction::Delete)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(ctx.video_playlist.borrow().len(), 3);
        assert!(pane.video_marked.is_empty());
        let remaining: Vec<String> =
            ctx.video_playlist.borrow().iter().map(|e| e.url.clone()).collect();
        assert_eq!(remaining, vec!["url0", "url1", "url5"]);
        // The selection lands on the row that took the removed selection's
        // place (clamped to the list).
        assert_eq!(pane.video_state.selected(), Some(2));
    }

    #[test]
    fn esc_with_a_selection_consumes_the_keypress() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        pane.video_state.select(Some(2));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3]);

        // Esc carries both Close (menu) and ShowSettings; clearing the
        // selection must consume the keypress so settings does not open on
        // the same press.
        let mut ev = action(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.video_marked.is_empty(), "Esc clears the selection");
        assert!(ev.is_consumed(), "clearing the selection consumes the keypress");
    }

    #[test]
    fn esc_without_a_selection_leaves_settings_enabled() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let mut ev = action(vec![
            Actions::Common(CommonAction::Close),
            Actions::Global(GlobalAction::ShowSettings),
        ]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.video_marked.is_empty());
        assert!(!ev.is_consumed(), "no selection: Esc still opens settings");
    }

    #[test]
    fn shift_range_reanchors_after_esc_clears_the_selection() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(8));
        let mut pane = video_pane(&mut ctx);

        // Move the cursor to row 2, then Shift+Down twice marks [2..=4]
        // (anchor 2).
        for _ in 0..3 {
            let mut ev = action(vec![Actions::Common(CommonAction::Down)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        for _ in 0..2 {
            let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
            pane.handle_action(&mut ev, &mut ctx).unwrap();
        }
        assert_eq!(pane.video_state.selected(), Some(4));
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![2, 3, 4]);

        // Esc clears the marks - and (with the fix) the anchor too.
        let mut ev = action(vec![Actions::Common(CommonAction::Close)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(pane.video_marked.is_empty());

        // Move the cursor back up to row 3, then Shift+Down: the range
        // must start from the new cursor (3), not reach back to the old
        // anchor (2).
        let mut ev = action(vec![Actions::Common(CommonAction::Up)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_state.selected(), Some(3));
        let mut ev = action(vec![Actions::Common(CommonAction::SelectDown)]);
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert_eq!(pane.video_state.selected(), Some(4));
        assert_eq!(pane.video_marked.iter().collect::<Vec<_>>(), vec![3, 4]);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod hover_render_tests {
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    use super::{Areas, Pane, QueuePane};
    use crate::{
        shared::mouse_event::MouseEventKind,
        tests::fixtures::ctx as fixture_ctx,
    };

    fn queue_ctx() -> (crate::ctx::Ctx, crossbeam::channel::Sender<crate::shared::events::AppEvent>) {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = fixture_ctx(
            (app_tx.clone(), _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_tx)
    }

    fn entries(n: usize) -> Vec<crate::core::mpv::MpvPlaylistEntry> {
        (0..n)
            .map(|i| crate::core::mpv::MpvPlaylistEntry::new(format!("v{i}"), format!("url{i}"), None))
            .collect()
    }

    fn video_pane(ctx: &mut crate::ctx::Ctx) -> QueuePane {
        ctx.queue_tab.set(crate::ctx::QueueTabMode::Video);
        let mut pane = QueuePane::new(ctx);
        pane.video_items_len = ctx.video_playlist.borrow().len();
        pane
    }

    /// Render the pane once and return the table area (so the row maths
    /// below line up with the drawn rows).
    fn render(pane: &mut QueuePane, ctx: &crate::ctx::Ctx) -> Rect {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        pane.areas[Areas::Table]
    }

    fn row_bg(
        pane: &mut QueuePane,
        ctx: &crate::ctx::Ctx,
        table: Rect,
        row: u16,
    ) -> Option<ratatui::style::Color> {
        let backend = TestBackend::new(80, 24);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| pane.render(frame, Rect::new(0, 0, 80, 24), ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        buf[(table.x + 1, table.y + row)].style().bg
    }

    #[test]
    fn video_row_under_mouse_gets_the_hover_highlight() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);
        let table = render(&mut pane, &ctx);

        // No mouse: the row background is the plain list text (a Reset bg).
        assert_eq!(row_bg(&mut pane, &ctx, table, 2), Some(ratatui::style::Color::Reset));

        // Point the mouse at row 2: the row gets the hover highlight.
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: table.x + 1, y: table.y + 2 }));
        let hovered = ctx.config.theme.hovered_item_style.bg;
        assert_eq!(row_bg(&mut pane, &ctx, table, 2), hovered);

        // The other rows stay plain.
        assert_eq!(row_bg(&mut pane, &ctx, table, 3), Some(ratatui::style::Color::Reset));
        ctx.set_mouse_pos(None);
    }

    #[test]
    fn hovering_the_selected_video_row_uses_the_hover_highlight() {
        let (mut ctx, _tx) = queue_ctx();
        ctx.video_playlist.borrow_mut().extend(entries(6));
        let mut pane = video_pane(&mut ctx);

        // Select row 2 (the keyboard cursor); hovering it must still show
        // the hover highlight (brighter than the plain selection).
        let table = render(&mut pane, &ctx);
        pane.video_state.select(Some(2));
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: table.x + 1, y: table.y + 2 }));
        let hovered = ctx.config.theme.hovered_item_style.bg;
        assert_eq!(
            row_bg(&mut pane, &ctx, table, 2),
            hovered,
            "hover wins over the selection highlight on the selected row"
        );
        ctx.set_mouse_pos(None);
    }

    #[test]
    fn moved_events_are_tracked_by_the_mouse_event_tracker() {
        let mut tracker = crate::shared::mouse_event::MouseEventTracker::default();
        let ev = tracker.track_and_get(crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: 7,
            row: 3,
            modifiers: crossterm::event::KeyModifiers::NONE,
        });
        let ev = ev.expect("moves produce an event");
        assert!(matches!(ev.kind, MouseEventKind::Moved));
        assert_eq!((ev.x, ev.y), (7, 3));
    }
}
