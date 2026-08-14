use std::collections::HashSet;

use anyhow::Result;
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::Stylize,
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph},
};

use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction,
            DirectoriesActions,
            GlobalAction,
            actions::{AddKind, AutoplayKind, DeleteKind, Position, SaveKind},
        },
        tabs::{PaneType, TreeBrowserArgs},
    },
    core::command::{create_env, run_external},
    ctx::{Ctx, LIKE_STICKER, RATING_STICKER},
    mpd::{
        client::Client,
        commands::Song,
        mpd_client::{Filter, FilterKind, MpdClient, MpdCommand},
        proto_client::ProtoClient,
        version::Version,
    },
    shared::{
        keys::ActionEvent,
        macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        UiEvent,
        dirstack::Dir,
        song_list::SongListCore,
        input::InputResultEvent,
        modals::{
            input_modal::InputModal,
            menu::{
                add_to_playlist_or_show_modal,
                create_delete_modal,
                create_save_modal,
                delete_from_playlist_or_show_confirmation,
                modal::MenuModal,
            },
            select_modal::SelectModal,
        },
        panes::search::inputs::{ActionResult, InputGroups, InputType, TextboxInput},
        widgets::browser::BrowserArea,
    },
};

mod inputs;

#[derive(Debug)]
pub struct SearchPane {
    inputs: InputGroups,
    phase: Phase,
    songs_dir: Dir<Song, ListState>,
    column_areas: EnumMap<BrowserArea, Rect>,
}

/// The query id of the search's MPD queries. `pub(crate)` so the MPD
/// tab (which hosts the folded-in search since round 28) can forward
/// the results to it.
pub(crate) const SEARCH: &str = "search";

impl SearchPane {
    pub fn new(ctx: &Ctx) -> Self {
        let config = &ctx.config;

        let inputs = InputGroups::builder()
            .ctx(ctx)
            .search_config(&config.search)
            .initial_fold_case(!config.search.case_sensitive)
            .initial_strip_diacritics(config.search.ignore_diacritics)
            .search_button(config.search.search_button)
            .text_style(config.as_list_name_style())
            .separator_style(config.theme.borders_style)
            .current_item_style(config.theme.current_item_style)
            .highlight_item_style(config.theme.highlighted_item_style)
            // The focused filter input uses the hover highlight while the
            // filter pane holds the keyboard cursor (Radio/Playlists focus
            // convention).
            .focused_style(config.theme.hovered_item_style)
            .stickers_supported(ctx.stickers_supported.into())
            .strip_diacritics_supported(ctx.mpd_version >= Version::new(0, 25, 0))
            .custom_query(config.search.custom_query)
            .build();

        Self {
            phase: Phase::Search,
            songs_dir: Dir::default(),
            inputs,
            column_areas: EnumMap::default(),
        }
    }

    /// The buffer id of the first textbox filter (test-only accessor:
    /// the MPD tab's round-28 tests pin the session-lifetime filter state
    /// across Library↔Search toggles through it).
    #[cfg(test)]
    pub(crate) fn first_filter_buffer_id(&self) -> Option<crate::ui::input::BufferId> {
        self.inputs.inputs.iter().find_map(|input| match input {
            crate::ui::panes::search::inputs::InputType::Textbox(t)
            | crate::ui::panes::search::inputs::InputType::Numberbox(t) => Some(t.buffer_id),
            _ => None,
        })
    }

    fn items<'a>(&'a self, all: bool) -> Box<dyn Iterator<Item = (usize, &'a Song)> + 'a> {
        if all {
            Box::new(self.songs_dir.items.iter().enumerate())
        } else if !self.songs_dir.marked().is_empty() {
            Box::new(self.songs_dir.marked().iter().map(|idx| (*idx, &self.songs_dir.items[*idx])))
        } else if let Some(item) = self.songs_dir.selected_with_idx() {
            Box::new(std::iter::once(item))
        } else {
            Box::new(std::iter::empty())
        }
    }

    fn enqueue(&self, all: bool) -> (Option<usize>, Vec<Enqueue>) {
        let items = self
            .items(all)
            .map(|(_, item)| Enqueue::File { path: item.file.clone() })
            .collect_vec();

        let hovered = self.songs_dir.selected().map(|s| s.file.as_str());
        let hovered_idx = if let Some(hovered) = hovered {
            items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    if let Enqueue::File { path } = item { Some((idx, path)) } else { None }
                })
                .find(|(_, path)| path == &hovered)
                .map(|(idx, _)| idx)
        } else {
            None
        };

        (hovered_idx, items)
    }

    fn render_song_column(
        &mut self,
        frame: &mut ratatui::prelude::Frame<'_>,
        area: ratatui::prelude::Rect,
        ctx: &Ctx,
    ) {
        let config = &ctx.config;
        let column_right_padding: u16 = config.theme.scrollbar.is_some().into();
        let title = self.songs_dir.filter_text(area.width, ctx);

        let block = {
            let mut b = Block::default()
                .borders(Borders::ALL)
                .border_style(config.as_border_style())
                .title(" Results ");
            if let Some(title) = title {
                b = b.title(title);
            }
            b.padding(Padding::new(0, column_right_padding, 0, 0))
        };
        // The results pane holds the keyboard cursor in the BrowseResults
        // phase: its selection renders with the hover highlight even
        // without the mouse (Radio/Playlists focus convention).
        let results_focused = matches!(self.phase, Phase::BrowseResults);
        let directory = &mut self.songs_dir;

        directory.state.set_content_and_viewport_len(directory.items.len(), area.height.into());
        if !directory.items.is_empty() && directory.state.get_selected().is_none() {
            directory.state.select(Some(0), 0);
        }
        let inner_block = block.inner(area);

        self.column_areas[BrowserArea::Current] = inner_block;
        self.column_areas[BrowserArea::Scrollbar] = area;

        // The row under the mouse gets the hover highlight (slightly
        // brighter than the keyboard selection, dimmer than marked rows);
        // marked rows keep their marked highlight on hover. Rows marked via
        // to_list_items already carry the marked style; the hover style is
        // applied on top for the single row under the pointer.
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner_block,
            directory.state.inner.offset(),
            directory.items.len(),
            1,
        );
        let mut items =
            directory.to_list_items(ctx.config.theme.browser_song_format.0.as_slice(), ctx);
        if let Some(hover) = hover_idx
            && !directory.marked().contains(&hover)
        {
            items[hover] = items[hover].clone().style(config.theme.hovered_item_style);
        }
        let current = crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
            .highlight_style(if hover_idx == directory.state.get_selected() || results_focused {
                config.theme.hovered_item_style
            } else {
                config.theme.current_item_style
            })
            .style(config.as_list_name_style());
        frame.render_widget(block, area);
        frame.render_stateful_widget(current, inner_block, directory.state.as_render_state_ref());
        if let Some(scrollbar) = config.as_styled_scrollbar() {
            frame.render_stateful_widget(
                scrollbar,
                self.column_areas[BrowserArea::Scrollbar],
                directory.state.as_scrollbar_state_ref(),
            );
        }
    }

    /// Trigger search if search should be done on any change. Does nothing when
    /// a dedicated search button is used.
    fn maybe_search_on_change(&mut self, ctx: &Ctx) {
        if !ctx.config.search.search_button {
            self.search(ctx);
        }
    }

    fn search(&mut self, ctx: &Ctx) {
        let search_mode = self.inputs.search_mode();
        let filter = if let Some(custom_query) = self.inputs.custom_query(ctx)
            && !custom_query.is_empty()
        {
            vec![Filter::new(String::new(), "").with_type(FilterKind::CustomQuery(custom_query))]
        } else {
            self.inputs
                .inputs
                .iter()
                .filter_map(|input| match &input {
                    InputType::Textbox(TextboxInput {
                        buffer_id, filter_key: Some(key), ..
                    }) => {
                        let value = ctx.input.value(*buffer_id).trim().to_owned();
                        if !value.is_empty() && !key.is_empty() {
                            Some(Filter::new(key.to_owned(), value).with_type(search_mode.into()))
                        } else {
                            None
                        }
                    }
                    _ => None,
                })
                .collect_vec()
        };

        let stickers_supported = ctx.stickers_supported.into();
        let fold_case = self.inputs.fold_case();
        let strip_diacritics = self.inputs.strip_diacritics();
        let liked_filter = self.inputs.liked_filter();

        let rating_filter = if self.inputs.is_rating_filter_active() {
            let Ok(rating_filter) = self.inputs.rating_filter(ctx) else {
                status_error!("Rating must be a valid integer {:?}", self.inputs.rating_value(ctx));
                return;
            };
            rating_filter
        } else {
            None
        };

        if filter.is_empty()
            && stickers_supported
            && (rating_filter.is_some() || liked_filter.is_some())
        {
            // Filters are empty, but rating filters are set - show all songs with the
            // wanted rating
            // Round 28: the search UI folded into the MPD tab, so its
            // queries target the Directories pane (the results land in the
            // MPD tab's embedded search via `DirectoriesPane::on_query_finished`).
            ctx.query().id(SEARCH).replace_id(SEARCH).target(PaneType::Directories { tree: TreeBrowserArgs::default() }).query(
                move |client| {
                    // empty URI returns all songs with the sticker
                    let uris = match (rating_filter, liked_filter) {
                        (Some(rf), Some(lf)) => {
                            let mut ratings: HashSet<_> = client
                                .find_stickers("", RATING_STICKER, Some(rf))?
                                .0
                                .into_iter()
                                .map(|s| s.file)
                                .collect();
                            let liked: HashSet<_> = client
                                .find_stickers("", LIKE_STICKER, Some(lf))?
                                .0
                                .into_iter()
                                .map(|s| s.file)
                                .collect();

                            // Do an intersection of both sets
                            ratings.retain(|uri| liked.contains(uri));

                            ratings
                        }
                        (Some(rf), None) => client
                            .find_stickers("", RATING_STICKER, Some(rf))?
                            .0
                            .into_iter()
                            .map(|s| s.file)
                            .collect(),
                        (None, Some(lf)) => client
                            .find_stickers("", LIKE_STICKER, Some(lf))?
                            .0
                            .into_iter()
                            .map(|s| s.file)
                            .collect(),
                        (None, None) => HashSet::new(),
                    };

                    client.send_start_cmd_list()?;
                    for uri in uris {
                        client.send_lsinfo(Some(&uri))?;
                    }
                    client.send_execute_cmd_list()?;
                    let data: Vec<Song> = client.read_response()?;

                    Ok(MpdQueryResult::SearchResult { data })
                },
            );
        } else if filter.is_empty() {
            // Filters are empty, stickers are either not supported or not set - clear
            // current results
            let _ = std::mem::take(&mut self.songs_dir);
        } else {
            // Search normally
            ctx.query().id(SEARCH).replace_id(SEARCH).target(PaneType::Directories { tree: TreeBrowserArgs::default() }).query(
                move |client| {
                    let data = if fold_case {
                        client.search(&filter, strip_diacritics)
                    } else {
                        client.find(&filter)
                    }?;

                    let data = if stickers_supported && rating_filter.is_some() {
                        // empty URI returns all songs with the sticker
                        let ratings = client.find_stickers("", RATING_STICKER, rating_filter)?;
                        let ratings: HashSet<_> = ratings.into_iter().map(|r| r.file).collect();
                        data.into_iter().filter(|song| ratings.contains(&song.file)).collect()
                    } else {
                        data
                    };

                    let data = if stickers_supported && liked_filter.is_some() {
                        // empty URI returns all songs with the sticker
                        let liked = client.find_stickers("", LIKE_STICKER, liked_filter)?;
                        let liked: HashSet<_> = liked.into_iter().map(|r| r.file).collect();
                        data.into_iter().filter(|song| liked.contains(&song.file)).collect()
                    } else {
                        data
                    };

                    Ok(MpdQueryResult::SearchResult { data })
                },
            );
        }
    }

    fn handle_search_phase_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        let config = &ctx.config;
        if let Some(action) = event.claim_global() {
            if let GlobalAction::ExternalCommand { command, .. } = action {
                let songs = self.songs_dir.items.iter().map(|song| song.file.as_str());
                run_external(command.clone(), create_env(ctx, songs));
            } else {
                event.abandon();
            }
        }

        // The directories keybind context carries `d`/`a`/`←`/`→`: `d`/`→`
        // move from the filter pane into the results list, `a`/`←` keep
        // the focus in the filters (no-op here). `w`/`s` keep their
        // navigation meaning through the common context below (the
        // FolderUp/FolderDown actions fall through via abandon).
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    if !self.songs_dir.items.is_empty() {
                        self.phase = Phase::BrowseResults;

                        ctx.render()?;
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => return Ok(()),
                _ => event.abandon(),
            }
        }

        if let Some(action) = event.claim_common() {
            match action.to_owned() {
                CommonAction::Down => {
                    if config.wrap_navigation {
                        self.inputs.next();
                    } else {
                        self.inputs.next_non_wrapping();
                    }

                    ctx.render()?;
                }
                CommonAction::Up => {
                    if config.wrap_navigation {
                        self.inputs.prev();
                    } else {
                        self.inputs.prev_non_wrapping();
                    }

                    ctx.render()?;
                }
                CommonAction::MoveDown => {}
                CommonAction::MoveUp => {}
                CommonAction::DownHalf => {}
                CommonAction::UpHalf => {}
                CommonAction::PageDown => {}
                CommonAction::PageUp => {}
                CommonAction::Right if !self.songs_dir.items.is_empty() => {
                    self.phase = Phase::BrowseResults;

                    ctx.render()?;
                }
                CommonAction::Right => {}
                CommonAction::Left => {}
                CommonAction::Top => {
                    self.inputs.first();

                    ctx.render()?;
                }
                CommonAction::Bottom => {
                    self.inputs.last();

                    ctx.render()?;
                }
                CommonAction::EnterSearch => {}
                CommonAction::NextResult => {}
                CommonAction::PreviousResult => {}
                CommonAction::Select => {}
                // Ctrl+A does not apply to the search filter pane (the
                // results list in the BrowseResults phase handles it).
                CommonAction::SelectAll => {}
                CommonAction::SelectDown => {}
                CommonAction::SelectUp => {}
                CommonAction::InvertSelection => {}
                CommonAction::Rename => {}
                CommonAction::Close => {}
                CommonAction::Confirm => {
                    match self.inputs.activate_focused(ctx) {
                        ActionResult::Search => {
                            self.search(ctx);
                        }
                        ActionResult::Reset => {
                            self.inputs.reset_focused(ctx);
                            self.songs_dir = Dir::default();
                        }
                        ActionResult::None => {}
                    }
                    ctx.render()?;
                }
                CommonAction::FocusInput => {
                    self.inputs.enter_insert_mode(ctx);
                    ctx.render()?;
                }
                // Modal while we are on search column does not support all options. It can
                // be implemented later.
                CommonAction::AddOptions { kind: AddKind::Modal(_) } => {}
                CommonAction::AddOptions { kind: AddKind::Action(opts) } if opts.all => {
                    let (_, enqueue) = self.enqueue(opts.all);
                    if !enqueue.is_empty() {
                        let current_song_idx = ctx.find_current_song_in_queue().map(|(i, _)| i);
                        Client::resolve_and_enqueue(
                            ctx,
                            enqueue,
                            opts.position,
                            opts.autoplay,
                            current_song_idx,
                            None,
                        );
                    }
                }
                // This action only makes sense when opts.all is true while we are on the
                // search column.
                CommonAction::AddOptions { kind: AddKind::Action(_) } => {}
                CommonAction::Delete => {
                    self.inputs.reset_focused(ctx);
                    self.songs_dir = Dir::default();
                    ctx.render()?;
                }
                CommonAction::PaneDown => {}
                CommonAction::PaneUp => {}
                CommonAction::PaneRight => {}
                CommonAction::PaneLeft => {}
                CommonAction::ShowInfo => {}
                CommonAction::ContextMenu => {}
                CommonAction::Rate { kind: _, min_rating: _, max_rating: _, current: true } => {
                    event.abandon();
                }
                CommonAction::Rate { .. } => {}
                CommonAction::Save {
                    kind: SaveKind::Playlist { name, all: true, duplicates_strategy },
                } => {
                    let song_paths: Vec<String> =
                        self.items(true).map(|(_, song)| song.file.clone()).collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to save");
                        return Ok(());
                    }

                    add_to_playlist_or_show_modal(name, song_paths, duplicates_strategy, ctx);
                }
                CommonAction::Save { kind: SaveKind::Modal { all: true, duplicates_strategy } } => {
                    let song_paths: Vec<String> =
                        self.items(true).map(|(_, song)| song.file.clone()).collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to save");
                        return Ok(());
                    }

                    let modal = create_save_modal(song_paths, None, duplicates_strategy, ctx)?;
                    modal!(ctx, modal);
                }
                CommonAction::Save { .. } => {}
                CommonAction::DeleteFromPlaylist {
                    kind: DeleteKind::Playlist { name, all: true, confirmation },
                } => {
                    let song_paths: HashSet<String> =
                        self.items(true).map(|(_, song)| song.file.clone()).collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to delete");
                        return Ok(());
                    }

                    delete_from_playlist_or_show_confirmation(
                        name,
                        &song_paths,
                        confirmation,
                        ctx,
                    )?;
                }
                CommonAction::DeleteFromPlaylist {
                    kind: DeleteKind::Modal { all: true, confirmation },
                } => {
                    let song_paths: HashSet<_> =
                        self.items(true).map(|(_, song)| song.file.clone()).collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to delete");
                        return Ok(());
                    }

                    let modal = create_delete_modal(song_paths, confirmation, ctx)?;
                    modal!(ctx, modal);
                }
                CommonAction::DeleteFromPlaylist { .. } => {}
                CommonAction::LyricsNudgeUp | CommonAction::LyricsNudgeDown
                | CommonAction::LyricsSave
                | CommonAction::LyricsDeleteLine
                | CommonAction::LyricsEditLine
                | CommonAction::LyricsInsertBefore
                | CommonAction::LyricsInsertAfter
                | CommonAction::LyricsLineTime
                | CommonAction::LyricsSaveAndExit => {}
            }
        }

        Ok(())
    }

    fn handle_result_phase_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        let Phase::BrowseResults = &mut self.phase else {
            return Ok(());
        };
        if let Some(action) = event.claim_global() {
            match action {
                GlobalAction::ExternalCommand { command, .. }
                    if !self.songs_dir.marked().is_empty() =>
                {
                    let songs = self.songs_dir.marked_items().map(|song| song.file.as_str());
                    run_external(command.clone(), create_env(ctx, songs));
                }
                GlobalAction::ExternalCommand { command, .. } => {
                    let selected = self.songs_dir.selected().map(|s| s.file.as_str());
                    run_external(command.clone(), create_env(ctx, selected));
                }
                // Space with a multi-selection opens the options menu
                // instead of toggling playback (a single selection or none
                // keeps the transport).
                GlobalAction::TogglePause if self.songs_dir.marked().len() > 1 => {
                    self.open_result_phase_context_menu(ctx);
                }
                _ => {
                    event.abandon();
                }
            }
        }

        // The directories keybind context carries `d`/`a`/`←`/`→` in the
        // results list too: `d`/`→` enqueue the highlighted result (same
        // as the Right arrow), `a`/`←` return to the filter pane.
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    let items = self.songs_dir.selected().map_or_else(Vec::new, |item| {
                        vec![Enqueue::File { path: item.file.clone() }]
                    });
                    if !items.is_empty() {
                        ctx.command(move |client| {
                            client.enqueue_multiple(items, None, None, false)?;
                            Ok(())
                        });
                    }
                    return Ok(());
                }
                DirectoriesActions::FolderCollapse => {
                    self.phase = Phase::Search;

                    ctx.render()?;
                    return Ok(());
                }
                _ => event.abandon(),
            }
        }

        if let Some(action) = event.claim_common() {
            match action.to_owned() {
                // The results list reuses the shared SongListCore arms
                // (navigation, marks, filtering, rate / save /
                // delete-from-playlist, add-options); only these actions
                // keep pane-specific semantics:
                CommonAction::Right => {
                    let items = self.songs_dir.selected().map_or_else(Vec::new, |item| {
                        vec![Enqueue::File { path: item.file.clone() }]
                    });
                    if !items.is_empty() {
                        ctx.command(move |client| {
                            client.enqueue_multiple(items, None, None, false)?;
                            Ok(())
                        });
                    }
                }
                CommonAction::Left => {
                    self.phase = Phase::Search;

                    ctx.render()?;
                }
                CommonAction::Delete => {}
                // Enter opens the options menu (the right-click menu), like
                // the other list panes; playing happens via the menu's Play
                // item or `d`/`→`/double-click.
                CommonAction::Confirm => {
                    self.open_result_phase_context_menu(ctx);
                }
                CommonAction::ContextMenu => {
                    self.open_result_phase_context_menu(ctx);
                }
                other => self.handle_claimed_common_action(other, event, ctx)?,
            }
        }

        Ok(())
    }

    fn open_result_phase_context_menu(&self, ctx: &Ctx) {
        let modal = MenuModal::new(ctx)
            .list_section(ctx, move |mut section| {
                if !self.songs_dir.items.is_empty() {
                    // Marked (or the highlighted result) bulk actions first:
                    // add/replace the queue with exactly the selected rows.
                    let (_, selected_enqueue) = self.enqueue(false);
                    if !selected_enqueue.is_empty() {
                        let enqueue_clone = selected_enqueue.clone();
                        section.add_item("Add to queue", move |ctx| {
                            ctx.command(move |client| {
                                client.enqueue_multiple(enqueue_clone, None, None, false)?;
                                Ok(())
                            });
                            Ok(())
                        });
                        let enqueue_clone = selected_enqueue.clone();
                        section.add_item("Replace queue", move |ctx| {
                            ctx.command(move |client| {
                                client.enqueue_multiple(enqueue_clone, None, None, true)?;
                                Ok(())
                            });
                            Ok(())
                        });
                    }
                    let (_, enqueue) = self.enqueue(true);
                    if !enqueue.is_empty() {
                        let enqueue_clone = enqueue.clone();
                        section.add_item("Add all to queue", move |ctx| {
                            ctx.command(move |client| {
                                client.enqueue_multiple(enqueue_clone, None, None, false)?;
                                Ok(())
                            });
                            Ok(())
                        });
                        section.add_item("Replace queue with all", move |ctx| {
                            ctx.command(move |client| {
                                client.enqueue_multiple(enqueue, None, None, true)?;
                                Ok(())
                            });
                            Ok(())
                        });

                        let song_files =
                            self.items(true).map(|(_, item)| item.file.clone()).collect();
                        section.add_item("Create playlist from all", move |ctx| {
                            modal!(
                                ctx,
                                InputModal::new(ctx)
                                    .title("Create new playlist")
                                    .confirm_label("Save")
                                    .input_label("Playlist name:")
                                    .on_confirm(move |ctx, value| {
                                        let value = value.to_owned();
                                        ctx.command(move |client| {
                                            client.create_playlist(&value, song_files)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    })
                            );
                            Ok(())
                        });

                        let song_files =
                            self.items(true).map(|(_, item)| item.file.clone()).collect();
                        section.add_item("Add all to playlist", move |ctx| {
                            // The radio favourites playlist is Radio-tab-owned:
                            // it never appears as an add target.
                            let radio_playlist = ctx.config.radio.playlist.clone();
                            let playlists = ctx.query_sync(move |client| {
                                Ok(client
                                    .picker_playlists(&radio_playlist)?
                                    .into_iter()
                                    .map(|p| p.name)
                                    .collect_vec())
                            })?;
                            modal!(
                                ctx,
                                SelectModal::builder()
                                    .ctx(ctx)
                                    .options(playlists)
                                    .confirm_label("Add")
                                    .title("Select a playlist")
                                    .on_confirm(move |ctx, selected, _idx| {
                                        ctx.command(move |client| {
                                            client
                                                .add_to_playlist_multiple(&selected, song_files)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    })
                                    .build()
                            );
                            Ok(())
                        });
                    }
                }
                Some(section)
            })
            .list_section(ctx, |mut section| {
                // Play the marked/selected results (the old Enter behavior;
                // Enter itself now opens this menu).
                let play_items = self.enqueue(false);
                if !play_items.1.is_empty() {
                    section.add_item("Play", move |ctx| {
                        let (hovered_song_idx, items) = play_items.clone();
                        let current_song_idx = ctx.find_current_song_in_queue().map(|(i, _)| i);
                        if !items.is_empty() {
                            Client::resolve_and_enqueue(
                                ctx,
                                items,
                                Position::Replace,
                                AutoplayKind::Hovered,
                                current_song_idx,
                                hovered_song_idx,
                            );
                        }
                        Ok(())
                    });
                }

                let song_files = self.items(false).map(|(_, item)| item.file.clone()).collect();
                section.add_item("Create playlist", move |ctx| {
                    modal!(
                        ctx,
                        InputModal::new(ctx)
                            .title("Create new playlist")
                            .confirm_label("Save")
                            .input_label("Playlist name:")
                            .on_confirm(move |ctx, value| {
                                let value = value.to_owned();
                                ctx.command(move |client| {
                                    client.create_playlist(&value, song_files)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                    );
                    Ok(())
                });

                let song_files = self.items(false).map(|(_, item)| item.file.clone()).collect();
                section.add_item("Add to playlist", move |ctx| {
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
                    modal!(
                        ctx,
                        SelectModal::builder()
                            .ctx(ctx)
                            .options(playlists)
                            .confirm_label("Add")
                            .title("Select a playlist")
                            .on_confirm(move |ctx, selected, _idx| {
                                ctx.command(move |client| {
                                    client.add_to_playlist_multiple(&selected, song_files)?;
                                    Ok(())
                                });
                                Ok(())
                            })
                            .build()
                    );
                    Ok(())
                });
                Some(section)
            })
            .list_section(ctx, |mut section| {
                section.add_item("Cancel", |_| Ok(()));
                Some(section)
            })
            .build();
        modal!(ctx, modal);
    }

    fn scrollbar_area(&self) -> Option<Rect> {
        let area = self.column_areas[BrowserArea::Scrollbar];
        if area.width > 0 { Some(area) } else { None }
    }

    fn handle_scrollbar_interaction(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<bool> {
        if !matches!(self.phase, Phase::BrowseResults) {
            return Ok(false);
        }
        let Some(_) = ctx.config.theme.scrollbar else {
            return Ok(false);
        };
        let Some(scrollbar_area) = self.scrollbar_area() else {
            return Ok(false);
        };
        if !matches!(event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }) {
            return Ok(false);
        }

        let content_len = self.songs_dir.items.len();
        let viewport_len =
            self.songs_dir.state.viewport_len().unwrap_or(scrollbar_area.height as usize);
        // The rendered scrollbar's content_length is max_offset + 1, so the
        // geometry (thumb size / travel) matches the widget exactly.
        let content_len = content_len.saturating_sub(viewport_len).saturating_add(1).max(1);
        let position = self.songs_dir.state.inner.offset();
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        if let Some(perc) = self.songs_dir.state.scrollbar_drag.handle(
            event,
            scrollbar_area,
            content_len,
            viewport_len,
            position,
            begin_len,
            end_len,
        ) {
            self.songs_dir.scroll_to(perc, ctx.config.scrolloff);
            ctx.render()?;
            return Ok(true);
        }

        Ok(false)
    }
}

impl SongListCore<Song, ListState> for SearchPane {
    fn list(&self) -> &Dir<Song, ListState> {
        &self.songs_dir
    }

    fn list_mut(&mut self) -> &mut Dir<Song, ListState> {
        &mut self.songs_dir
    }

    fn list_songs_in_item(
        &self,
        item: Song,
    ) -> impl FnOnce(&mut Client<'_>) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
        move |_client| Ok(vec![item])
    }
}

impl Pane for SearchPane {
    fn render(
        &mut self,
        frame: &mut ratatui::prelude::Frame,
        area: ratatui::prelude::Rect,
        ctx: &Ctx,
    ) -> anyhow::Result<()> {
        // Same structure as the Directories / Playlists / Radio tabs: the
        // filter pane on the left, results + tips + info on the right.
        let [search_area, right] = Layout::horizontal([
            Constraint::Percentage(30),
            Constraint::Percentage(70),
        ])
        .areas(area);
        let [list_area, tips_area, info_area] = Layout::vertical([
            Constraint::Percentage(60),
            Constraint::Length(3),
            Constraint::Percentage(33),
        ])
        .areas(right);

        // Left: the search filters inside a bordered pane (always visible,
        // in both phases).
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Search ");
        let inner = block.inner(search_area);
        // The filter pane holds the keyboard cursor in the Search phase:
        // its focused input renders with the hover highlight then.
        self.inputs.render(inner, frame.buffer_mut(), ctx, matches!(self.phase, Phase::Search));
        frame.render_widget(block, search_area);
        self.column_areas[BrowserArea::Previous] = inner;

        // Right-top: the results list.
        self.render_song_column(frame, list_area, ctx);

        // Tips strip between the results and the info box.
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let tips = vec![
            Line::from(vec![
                Span::styled("w/s · ↑/↓", base),
                Span::styled("  filters · results", dim),
            ]),
            Line::from(vec![
                Span::styled("Enter", base),
                Span::styled("  options menu · d/→ play", dim),
            ]),
            Line::from(vec![
                Span::styled("Shift+↑/↓", base),
                Span::styled("  multi-select results", dim),
            ]),
        ];
        frame.render_widget(
            Paragraph::new(tips).style(dim),
            tips_area.inner(Margin { horizontal: 1, vertical: 0 }),
        );

        // Right-bottom: the selected song's info (yellow group labels, like
        // the Directories / Radio info boxes).
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(info_area);
        if let Some(song) = self.songs_dir.selected() {
            let preview = song.to_preview(
                ctx.config.theme.preview_label_style,
                ctx.config.theme.preview_metadata_group_style,
                ctx,
            );
            let mut result = Vec::new();
            for group in preview {
                if let Some(name) = group.name {
                    result.push(ListItem::new(name).yellow().bold());
                }
                result.extend(group.items.clone());
                result.push(ListItem::new(Span::raw("")));
            }
            let preview = List::new(result).style(ctx.config.as_list_name_style());
            frame.render_widget(preview, inner);
        }
        frame.render_widget(block, info_area);
        self.column_areas[BrowserArea::Preview] = inner;

        Ok(())
    }

    fn on_event(&mut self, event: &mut UiEvent, _is_visible: bool, ctx: &Ctx) -> Result<()> {
        match event {
            UiEvent::Database => {
                self.songs_dir = Dir::default();
                self.phase = Phase::Search;

                status_warn!(
                    "The music database has been updated. The current tab has been reinitialized in the root directory to prevent inconsistent behaviours."
                );
            }
            UiEvent::Reconnected => {
                self.phase = Phase::Search;
                self.songs_dir = Dir::default();
            }
            UiEvent::ConfigChanged => {
                *self = Self::new(ctx);
            }
            _ => {}
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
            (SEARCH, MpdQueryResult::SearchResult { data }) => {
                status_info!("Found {} matching songs", data.len());
                self.songs_dir = Dir::new(data);
                ctx.render()?;
            }
            _ => {}
        }
        Ok(())
    }

    fn handle_mouse_event(&mut self, event: MouseEvent, ctx: &Ctx) -> Result<()> {
        if self.handle_scrollbar_interaction(event, ctx)? {
            return Ok(());
        }

        match event.kind {
            MouseEventKind::LeftClick
                if self.column_areas[BrowserArea::Previous].contains(event.into()) =>
            {
                self.phase = Phase::Search;
                self.inputs.focus_input_at(event.into());
                ctx.render()?;
            }
            MouseEventKind::LeftClick
                if self.column_areas[BrowserArea::Preview].contains(event.into()) =>
            {
                match self.phase {
                    Phase::Search => {
                        // Clicking the info box moves to the results.
                        if !self.songs_dir.items.is_empty() {
                            self.phase = Phase::BrowseResults;
                        }
                        ctx.render()?;
                    }
                    Phase::BrowseResults => {
                        let (_, items) = self.enqueue(false);
                        if !items.is_empty() {
                            ctx.command(move |client| {
                                client.enqueue_multiple(items, None, None, false)?;
                                Ok(())
                            });
                        }
                    }
                }
            }
            MouseEventKind::LeftClick
                if self.column_areas[BrowserArea::Current].contains(event.into()) =>
            {
                // Clicking the results list moves the keyboard cursor
                // there (from the filter pane) and applies the shared
                // selection semantics: ctrl+click toggles the row's mark,
                // alt+click range-marks from the anchor, a plain click
                // drops the multi-selection, re-anchors and selects the row
                // (the queue audio list / MPD right pane behavior). In the
                // Search phase the click also leaves insert mode (the
                // filter edits stop) and re-runs the search.
                if matches!(self.phase, Phase::Search) {
                    if ctx.input.is_insert_mode() {
                        ctx.input.normal_mode();
                        self.maybe_search_on_change(ctx);
                    }
                    if self.songs_dir.items.is_empty() {
                        ctx.render()?;
                        return Ok(());
                    }
                    self.phase = Phase::BrowseResults;
                }
                let clicked_row = event
                    .y
                    .saturating_sub(self.column_areas[BrowserArea::Current].y)
                    .into();
                if let Some(idx) = self.songs_dir.state.get_at_rendered_row(clicked_row) {
                    if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
                        // Additive selection: the row under the cursor
                        // joins the marks too (a ctrl+click must never
                        // drop the initially selected item), and the
                        // clicked row is added without toggling off.
                        if self.songs_dir.state.marked.is_empty() {
                            if let Some(sel) = self.songs_dir.state.get_selected() {
                                self.songs_dir.state.mark(sel);
                            }
                        }
                        self.songs_dir.state.mark(idx);
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                    } else if event.modifiers.contains(crossterm::event::KeyModifiers::ALT) {
                        if self.songs_dir.state.mark_anchor().is_none() {
                            self.songs_dir.state.set_mark_anchor(idx);
                        }
                        // Replace the previous alt/shift range, so
                        // alt+clicking closer to the anchor deselects the
                        // entries beyond it.
                        if let Some((lo, hi)) = self.songs_dir.state.take_range_mark() {
                            for i in lo..=hi {
                                self.songs_dir.state.marked.remove(&i);
                            }
                        }
                        let anchor = self.songs_dir.state.mark_anchor().unwrap_or(idx);
                        let (lo, hi) = (anchor.min(idx), anchor.max(idx));
                        if lo < hi {
                            self.songs_dir.state.mark_range(lo, hi);
                            self.songs_dir.state.set_range_mark(lo, hi);
                        }
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                    } else {
                        // A plain click on a different row drops the
                        // multi-selection; clicking the selected row keeps
                        // it. The click becomes the new anchor.
                        if !self.songs_dir.marked().is_empty()
                            && Some(idx) != self.songs_dir.state.get_selected()
                        {
                            self.songs_dir.marked_mut().clear();
                        }
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                        self.songs_dir.state.set_mark_anchor(idx);
                        self.songs_dir.state.clear_range_mark();
                    }
                }
                ctx.render()?;
            }
            MouseEventKind::DoubleClick => match self.phase {
                Phase::Search => {
                    // Double-click on the filter pane activates the focused
                    // input (Search / Reset).
                    if self.column_areas[BrowserArea::Previous].contains(event.into()) {
                        match self.inputs.activate_focused(ctx) {
                            ActionResult::Search => {
                                self.search(ctx);
                            }
                            ActionResult::Reset => {
                                self.inputs.reset_focused(ctx);
                                self.songs_dir = Dir::default();
                            }
                            ActionResult::None => {}
                        }
                    }
                    ctx.render()?;
                }
                Phase::BrowseResults => {
                    let (_, items) = self.enqueue(false);
                    if !items.is_empty() {
                        ctx.command(move |client| {
                            client.enqueue_multiple(items, None, None, false)?;
                            Ok(())
                        });
                    }
                }
            },
            MouseEventKind::MiddleClick
                if self.column_areas[BrowserArea::Current].contains(event.into()) =>
            {
                match self.phase {
                    Phase::Search => {}
                    Phase::BrowseResults => {
                        let clicked_row = event
                            .y
                            .saturating_sub(self.column_areas[BrowserArea::Current].y)
                            .into();
                        if let Some(idx) = self.songs_dir.state.get_at_rendered_row(clicked_row) {
                            self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                            self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                            if let Some(item) = self.songs_dir.selected() {
                                let item = item.file.clone();
                                ctx.command(move |client| {
                                    client.add(&item, None)?;
                                    status_info!("Added '{item}' to queue");
                                    Ok(())
                                });
                            }
                            ctx.render()?;
                        }
                    }
                }
            }
            MouseEventKind::ScrollDown => match self.phase {
                Phase::Search => {
                    if ctx.input.is_insert_mode() {
                        ctx.input.normal_mode();
                        self.phase = Phase::Search;
                        self.maybe_search_on_change(ctx);
                    }
                    self.inputs.next_non_wrapping();
                    ctx.render()?;
                }
                Phase::BrowseResults => {
                    // Round 32: the wheel scrolls the viewport only — the
                    // highlight stays put and may leave the visible area.
                    self.songs_dir.scroll_viewport(1, ctx.config.scroll_amount.max(1));
                    ctx.render()?;
                }
            },
            MouseEventKind::ScrollUp => match self.phase {
                Phase::Search => {
                    if ctx.input.is_insert_mode() {
                        ctx.input.normal_mode();
                        self.phase = Phase::Search;
                        self.maybe_search_on_change(ctx);
                    }
                    self.inputs.prev_non_wrapping();
                    ctx.render()?;
                }
                Phase::BrowseResults => {
                    // Round 32: the wheel scrolls the viewport only — the
                    // highlight stays put and may leave the visible area.
                    self.songs_dir.scroll_viewport(-1, ctx.config.scroll_amount.max(1));
                    ctx.render()?;
                }
            },
            MouseEventKind::RightClick => match self.phase {
                Phase::BrowseResults if !ctx.input.is_active(self.songs_dir.filter_buffer_id) => {
                    let clicked_row =
                        event.y.saturating_sub(self.column_areas[BrowserArea::Current].y).into();
                    if let Some(idx) = self.songs_dir.state.get_at_rendered_row(clicked_row) {
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                        ctx.render()?;
                    }
                    self.open_result_phase_context_menu(ctx);
                }
                _ => {}
            },
            MouseEventKind::Drag { .. } => {
                // drag events are handled by scrollbar interaction, no
                // additional action needed
            }
            _ => {}
        }

        Ok(())
    }

    fn handle_insert_mode(&mut self, kind: InputResultEvent, ctx: &mut Ctx) -> Result<()> {
        match self.phase {
            Phase::Search => match kind {
                InputResultEvent::Push => {}
                InputResultEvent::Pop => {}
                InputResultEvent::Confirm => {
                    self.maybe_search_on_change(ctx);
                }
                InputResultEvent::NoChange => {}
                InputResultEvent::Cancel => {
                    self.maybe_search_on_change(ctx);
                }
            },
            Phase::BrowseResults => {
                let song_format = ctx.config.theme.browser_song_format.0.as_slice();
                match kind {
                    InputResultEvent::Push => {
                        self.songs_dir.recalculate_matched_items(song_format, ctx);
                        self.songs_dir.jump_first_matching(song_format, ctx);
                    }
                    InputResultEvent::Pop => {
                        self.songs_dir.recalculate_matched_items(song_format, ctx);
                    }
                    InputResultEvent::Confirm => {}
                    InputResultEvent::NoChange => {}
                    InputResultEvent::Cancel => {
                        self.songs_dir.set_filter_active(false);
                        ctx.input.clear_buffer(self.songs_dir.filter_buffer_id);
                    }
                }
            }
        }
        ctx.render()?;
        Ok(())
    }

    fn handle_action(&mut self, event: &mut ActionEvent, ctx: &mut Ctx) -> Result<()> {
        match &mut self.phase {
            Phase::Search => {
                self.handle_search_phase_action(event, ctx)?;
            }
            Phase::BrowseResults => {
                self.handle_result_phase_action(event, ctx)?;
            }
        }
        Ok(())
    }
}

#[derive(Debug)]
enum Phase {
    Search,
    BrowseResults,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crossterm::event::KeyModifiers;
    use ratatui::prelude::Rect;

    use super::{BrowserArea, Phase, SearchPane};
    use crate::{
        config::keys::CommonAction,
        mpd::commands::Song,
        shared::{
            keys::{ActionEvent, Actions},
            mouse_event::{MouseEvent, MouseEventKind},
        },
        ui::{dirstack::Dir, panes::Pane},
    };

    fn act(pane: &mut SearchPane, ctx: &mut crate::ctx::Ctx, actions: Vec<Actions>) {
        let mut event = ActionEvent::from(std::sync::Arc::new(actions));
        pane.handle_action(&mut event, ctx).unwrap();
    }

    fn songs() -> Vec<Song> {
        (0..5)
            .map(|i| Song {
                id: i as u32,
                file: format!("/mnt/music/{i}.flac"),
                duration: Some(Duration::from_secs(10)),
                ..Default::default()
            })
            .collect()
    }

    fn click(pane: &mut SearchPane, x: u16, y: u16, ctx: &mut crate::ctx::Ctx) {
        click_mod(pane, x, y, crossterm::event::KeyModifiers::NONE, ctx);
    }

    fn click_mod(
        pane: &mut SearchPane,
        x: u16,
        y: u16,
        modifiers: KeyModifiers,
        ctx: &mut crate::ctx::Ctx,
    ) {
        pane.handle_mouse_event(
            MouseEvent { x, y, kind: MouseEventKind::LeftClick, modifiers },
            ctx,
        )
        .unwrap();
    }

    /// Click row `row` of the results list (5 columns in, past the border).
    fn click_row(
        pane: &mut SearchPane,
        current: Rect,
        row: u16,
        modifiers: KeyModifiers,
        ctx: &mut crate::ctx::Ctx,
    ) {
        click_mod(pane, current.x + 5, current.y + row, modifiers, ctx);
    }

    fn make_ctx() -> crate::ctx::Ctx {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        )
    }

    fn marks(pane: &SearchPane) -> Vec<usize> {
        pane.songs_dir.marked().iter().copied().collect()
    }

    fn row_bg(
        pane: &mut SearchPane,
        ctx: &crate::ctx::Ctx,
        row: u16,
    ) -> Option<ratatui::style::Color> {
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        let current = pane.column_areas[BrowserArea::Current];
        buf[(current.x, current.y + row)].style().bg
    }

    fn input_bg(
        pane: &mut SearchPane,
        ctx: &crate::ctx::Ctx,
        area: Rect,
        row: u16,
    ) -> Option<ratatui::style::Color> {
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), ctx).unwrap())
            .unwrap();
        let buf = terminal.backend().buffer();
        buf[(area.x, area.y + row)].style().bg
    }

    /// Render the pane at a given size and return the buffer.
    fn render_buf(
        pane: &mut SearchPane,
        ctx: &crate::ctx::Ctx,
        w: u16,
        h: u16,
    ) -> ratatui::buffer::Buffer {
        let backend = ratatui::backend::TestBackend::new(w, h);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, w, h), ctx).unwrap())
            .unwrap();
        terminal.backend().buffer().clone()
    }

    fn rendered_pane(ctx: &mut crate::ctx::Ctx) -> SearchPane {
        let mut pane = SearchPane::new(ctx);
        let backend = ratatui::backend::TestBackend::new(100, 40);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal
            .draw(|frame| pane.render(frame, Rect::new(0, 0, 100, 40), ctx).unwrap())
            .unwrap();
        pane
    }

    #[test]
    fn clicking_a_result_selects_it_and_enters_browse_results() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        assert!(matches!(pane.phase, Phase::Search));

        // Click a results row (right pane, second row).
        let current = pane.column_areas[BrowserArea::Current];
        click(&mut pane, current.x + 5, current.y + 1, &mut ctx);

        assert!(matches!(pane.phase, Phase::BrowseResults));
        assert_eq!(pane.songs_dir.state.get_selected(), Some(1));
    }

    #[test]
    fn clicking_the_filter_pane_focuses_the_clicked_input() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut pane = rendered_pane(&mut ctx);
        let focused_before = pane.inputs.focused_idx();

        // Click the second row of the filter pane (the Artist field).
        let previous = pane.column_areas[BrowserArea::Previous];
        click(&mut pane, previous.x + 5, previous.y + 1, &mut ctx);

        assert!(matches!(pane.phase, Phase::Search));
        assert_eq!(pane.inputs.focused_idx(), 1, "clicking the second filter row focuses it");
        assert_ne!(pane.inputs.focused_idx(), focused_before);
    }

    #[test]
    fn test_search_pane_scrollbar_calculation() {
        let scrollbar_height: u16 = 10;
        let total_items: usize = 50;

        let clicked_y = scrollbar_height.saturating_sub(1);
        let target_idx = if clicked_y >= scrollbar_height.saturating_sub(1) {
            total_items.saturating_sub(1)
        } else {
            let position_ratio =
                f64::from(clicked_y) / f64::from(scrollbar_height.saturating_sub(1));
            ((position_ratio * (total_items.saturating_sub(1)) as f64) as usize)
                .min(total_items.saturating_sub(1))
        };

        assert_eq!(target_idx, total_items - 1);

        let clicked_y = 0;
        let position_ratio = f64::from(clicked_y) / f64::from(scrollbar_height.saturating_sub(1));
        let target_idx = ((position_ratio * (total_items.saturating_sub(1)) as f64) as usize)
            .min(total_items.saturating_sub(1));

        assert_eq!(target_idx, 0);

        let clicked_y = 5;
        let position_ratio = f64::from(clicked_y) / f64::from(scrollbar_height.saturating_sub(1));
        let target_idx = ((position_ratio * (total_items.saturating_sub(1)) as f64) as usize)
            .min(total_items.saturating_sub(1));

        // should be roughly in the middle (around 25-27)
        assert!((20..=30).contains(&target_idx));
    }

    #[test]
    fn ctrl_click_adds_to_the_selection_and_plain_click_clears() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;
        let current = pane.column_areas[BrowserArea::Current];

        // A plain click highlights row 0; ctrl+click rows 2 and 4 ADD to
        // it (the initially selected row stays marked).
        click_row(&mut pane, current, 0, KeyModifiers::NONE, &mut ctx);
        click_row(&mut pane, current, 2, KeyModifiers::CONTROL, &mut ctx);
        click_row(&mut pane, current, 4, KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(
            marks(&pane),
            vec![0, 2, 4],
            "ctrl+click keeps the initially selected row and adds the clicked rows"
        );
        // ctrl+click on an already-marked row keeps it (pure additive).
        click_row(&mut pane, current, 0, KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 2, 4]);
        // A plain click on a different row clears the whole selection.
        click_row(&mut pane, current, 3, KeyModifiers::NONE, &mut ctx);
        assert!(marks(&pane).is_empty(), "plain click clears the marks");
        assert_eq!(pane.songs_dir.state.get_selected(), Some(3));
    }

    #[test]
    fn ctrl_a_marks_every_result_in_browse_results_phase() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;

        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectAll)]);
        assert_eq!(marks(&pane), (0..5).collect::<Vec<_>>());

        // Ctrl+A again keeps everything marked.
        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectAll)]);
        assert_eq!(marks(&pane).len(), 5);
    }

    #[test]
    fn ctrl_a_in_the_filter_phase_is_a_no_op() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::Search;

        act(&mut pane, &mut ctx, vec![Actions::Common(CommonAction::SelectAll)]);
        assert!(
            pane.songs_dir.marked().is_empty(),
            "the MPD search filter pane is excluded from ctrl+a"
        );
    }

    #[test]
    fn alt_click_ranges_from_the_anchor() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;
        let current = pane.column_areas[BrowserArea::Current];

        // A plain click sets the anchor (row 0).
        click_row(&mut pane, current, 0, KeyModifiers::NONE, &mut ctx);
        // alt+click on row 3 range-marks [0..3].
        click_row(&mut pane, current, 3, KeyModifiers::ALT, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 1, 2, 3]);
        // alt+clicking closer to the anchor replaces the range.
        click_row(&mut pane, current, 1, KeyModifiers::ALT, &mut ctx);
        assert_eq!(marks(&pane), vec![0, 1]);
    }

    #[test]
    fn marked_rows_render_with_the_marked_style() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;
        let current = pane.column_areas[BrowserArea::Current];

        // Plain-click row 2, then ctrl+click row 4: both marked (the
        // initially selected row stays marked); the cursor lands on row 4.
        click_row(&mut pane, current, 2, KeyModifiers::NONE, &mut ctx);
        click_row(&mut pane, current, 4, KeyModifiers::CONTROL, &mut ctx);
        assert_eq!(marks(&pane), vec![2, 4]);

        // The marked row that is not the cursor renders with the lighter
        // marked highlight; the cursor row keeps the List's accent
        // highlight (the directories convention).
        let marked = ctx.config.theme.marked_item_style.bg;
        assert_eq!(
            row_bg(&mut pane, &ctx, 2),
            marked,
            "the marked row renders with the marked highlight"
        );
    }

    #[test]
    fn row_under_mouse_gets_the_hover_highlight() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;
        let current = pane.column_areas[BrowserArea::Current];

        // No mouse: the row background is the plain list text (a Reset bg).
        assert_eq!(row_bg(&mut pane, &ctx, 2), Some(ratatui::style::Color::Reset));

        // Point the mouse at row 2: the row gets the hover highlight.
        ctx.set_mouse_pos(Some(ratatui::layout::Position { x: current.x + 1, y: current.y + 2 }));
        let hovered = ctx.config.theme.hovered_item_style.bg;
        assert_eq!(row_bg(&mut pane, &ctx, 2), hovered);

        // The other rows stay plain.
        assert_eq!(row_bg(&mut pane, &ctx, 3), Some(ratatui::style::Color::Reset));
        ctx.set_mouse_pos(None);
    }

    #[test]
    fn focused_pane_selection_uses_the_hover_highlight() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        let hovered = ctx.config.theme.hovered_item_style.bg;
        let current = ctx.config.theme.current_item_style.bg;
        let inputs_area = pane.inputs.area;
        let focused_row = pane.inputs.focused_idx() as u16;

        // Search phase: the filter pane holds the cursor - the focused
        // input uses the hover highlight, the results list keeps the plain
        // selection.
        assert_eq!(
            input_bg(&mut pane, &ctx, inputs_area, focused_row),
            hovered,
            "the focused filter input uses the hover highlight"
        );
        assert_eq!(
            row_bg(&mut pane, &ctx, 0),
            current,
            "the results list keeps the plain selection in the Search phase"
        );

        // BrowseResults phase: the results pane holds the cursor - the
        // results selection uses the hover highlight, the filter input
        // keeps the plain selection.
        pane.phase = Phase::BrowseResults;
        assert_eq!(
            row_bg(&mut pane, &ctx, 0),
            hovered,
            "the results selection uses the hover highlight"
        );
        assert_eq!(
            input_bg(&mut pane, &ctx, inputs_area, focused_row),
            current,
            "the filter input keeps the plain selection"
        );
    }

    #[test]
    fn keyboard_d_and_arrows_switch_between_filters_and_results() {
        use std::sync::Arc;

        use crate::shared::keys::{ActionEvent, Actions};

        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        assert!(matches!(pane.phase, Phase::Search));

        // `d` (the directories FolderExpand key) enters the results.
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Directories(
            crate::config::keys::DirectoriesActions::FolderExpand,
        )]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(matches!(pane.phase, Phase::BrowseResults), "d enters the results");

        // `a` (FolderCollapse) returns to the filters.
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Directories(
            crate::config::keys::DirectoriesActions::FolderCollapse,
        )]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(matches!(pane.phase, Phase::Search), "a returns to the filters");

        // `→` (PlayFile) enters again; `←` (FolderCollapse) leaves again.
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Directories(
            crate::config::keys::DirectoriesActions::PlayFile,
        )]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(matches!(pane.phase, Phase::BrowseResults), "→ enters the results");
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Directories(
            crate::config::keys::DirectoriesActions::FolderCollapse,
        )]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(matches!(pane.phase, Phase::Search), "← returns to the filters");

        // With no results, `d` stays in the filters.
        pane.songs_dir = Dir::default();
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Directories(
            crate::config::keys::DirectoriesActions::FolderExpand,
        )]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(matches!(pane.phase, Phase::Search), "d with no results stays put");
    }

    #[test]
    fn filter_row_under_the_mouse_gets_the_hover_highlight() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        let hovered = ctx.config.theme.hovered_item_style.bg;
        let inputs_area = pane.inputs.area;

        // Hover the second filter row: it gets the hover highlight.
        ctx.set_mouse_pos(Some(ratatui::layout::Position {
            x: inputs_area.x + 1,
            y: inputs_area.y + 1,
        }));
        assert_eq!(
            input_bg(&mut pane, &ctx, inputs_area, 1),
            hovered,
            "the hovered filter row uses the hover highlight"
        );

        // The hover wins over the keyboard selection on the focused row.
        assert_eq!(
            input_bg(&mut pane, &ctx, inputs_area, 0),
            hovered,
            "hover wins over the keyboard selection"
        );

        // A row that is not under the mouse stays plain.
        ctx.set_mouse_pos(Some(ratatui::layout::Position {
            x: inputs_area.x + 1,
            y: inputs_area.y + 3,
        }));
        assert_ne!(
            input_bg(&mut pane, &ctx, inputs_area, 1),
            hovered,
            "a non-hovered row keeps its plain style"
        );

        // No mouse position: nothing is hovered.
        ctx.set_mouse_pos(None);
        assert_ne!(
            input_bg(&mut pane, &ctx, inputs_area, 1),
            hovered,
            "no pointer, no hover"
        );
    }

    #[test]
    fn narrow_pane_keeps_the_filter_values_visible() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        let buf = render_buf(&mut pane, &ctx, 80, 40);
        let inner = pane.column_areas[BrowserArea::Previous];

        // The first filter row's label ends with a colon; the cells after
        // it are the value column. On an 80-column terminal the pane must
        // keep that column wide enough to read a query, instead of cutting
        // it to zero (the round-26 regression).
        let mut colon = None;
        for x in inner.x..inner.x.saturating_add(inner.width) {
            if buf[(x, inner.y)].symbol() == ":" {
                colon = Some(x);
                break;
            }
        }
        let colon = colon.expect("the first filter row renders its label colon");
        let value_width = (inner.x + inner.width).saturating_sub(colon + 1);
        assert!(
            value_width >= 10,
            "the filter value column stays usable on a narrow pane (got {value_width} cells)"
        );
    }

    /// A ctx that keeps the app-event receiver, so tests can observe the
    /// modals the pane opens.
    fn make_ctx_with_rx() -> (crate::ctx::Ctx, crossbeam::channel::Receiver<crate::shared::events::AppEvent>) {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        (ctx, app_rx)
    }

    #[test]
    fn enter_opens_the_options_menu_in_browse_results() {
        use std::sync::Arc;

        use crate::config::keys::{CommonAction, GlobalAction};
        use crate::shared::keys::{ActionEvent, Actions};

        let (mut ctx, app_rx) = make_ctx_with_rx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;

        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Common(CommonAction::Confirm)]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();

        let got = app_rx.try_recv();
        assert!(
            matches!(got, Ok(crate::shared::events::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))),
            "Enter in the results opens the options menu (got {got:?})"
        );
    }

    #[test]
    fn space_with_multiple_marks_opens_the_menu_instead_of_pausing() {
        use std::sync::Arc;

        use crate::config::keys::{CommonAction, GlobalAction};
        use crate::shared::keys::{ActionEvent, Actions};

        let (mut ctx, app_rx) = make_ctx_with_rx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;
        pane.songs_dir.state.toggle_mark(0);
        pane.songs_dir.state.toggle_mark(2);
        assert_eq!(pane.songs_dir.marked().len(), 2);

        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Global(GlobalAction::TogglePause)]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();

        let got = app_rx.try_recv();
        assert!(
            matches!(got, Ok(crate::shared::events::AppEvent::UiEvent(crate::ui::UiAppEvent::Modal(_)))),
            "Space with a multi-selection opens the options menu (got {got:?})"
        );
    }

    #[test]
    fn space_with_single_or_no_marks_keeps_playback() {
        use std::sync::Arc;

        use crate::config::keys::{CommonAction, GlobalAction};
        use crate::shared::keys::{ActionEvent, Actions};

        let (mut ctx, app_rx) = make_ctx_with_rx();
        let mut pane = rendered_pane(&mut ctx);
        pane.songs_dir = Dir::new(songs());
        pane.phase = Phase::BrowseResults;

        // No marks: Space keeps the transport.
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Global(GlobalAction::TogglePause)]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(app_rx.try_recv().is_err(), "no marks: Space keeps the transport");

        // A single mark: Space still controls playback.
        pane.songs_dir.state.toggle_mark(1);
        let mut ev = ActionEvent::from(Arc::new(vec![Actions::Global(GlobalAction::TogglePause)]));
        pane.handle_action(&mut ev, &mut ctx).unwrap();
        assert!(app_rx.try_recv().is_err(), "single mark: Space keeps the transport");
    }

    #[test]
    fn expanded_filter_labels_keep_the_margin_and_value_spacing() {
        let mut ctx = make_ctx();
        let mut pane = rendered_pane(&mut ctx);
        // A wide terminal renders the expanded label form (" : ").
        let buf = render_buf(&mut pane, &ctx, 140, 40);
        let inner = pane.column_areas[BrowserArea::Previous];

        let mut colon = None;
        for x in inner.x..inner.x.saturating_add(inner.width) {
            if buf[(x, inner.y)].symbol() == ":" {
                colon = Some(x);
                break;
            }
        }
        let colon = colon.expect("the first filter row renders its label colon");
        // One space separates the colon from the value.
        assert_eq!(
            buf[(colon + 1, inner.y)].symbol(),
            " ",
            "the value is separated from the colon by a space"
        );
        let value_width = (inner.x + inner.width).saturating_sub(colon + 2);
        assert!(
            value_width >= 10,
            "the value column stays wide on the expanded pane (got {value_width})"
        );
    }
}
