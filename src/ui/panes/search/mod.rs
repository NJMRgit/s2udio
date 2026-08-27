use std::collections::HashSet;
use anyhow::Result;
use enum_map::EnumMap;
use itertools::Itertools;
use ratatui::{
    layout::{Constraint, Layout, Margin, Rect},
    style::Stylize, text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Padding, Paragraph},
};
use super::Pane;
use crate::{
    MpdQueryResult,
    config::{
        keys::{
            CommonAction, DirectoriesActions, GlobalAction,
            actions::{AddKind, AutoplayKind, DeleteKind, Position, SaveKind},
        },
        tabs::{PaneType, TreeBrowserArgs},
    },
    core::command::{create_env, run_external},
    ctx::{Ctx, LIKE_STICKER, RATING_STICKER},
    mpd::{
        client::Client, commands::Song,
        mpd_client::{Filter, FilterKind, MpdClient, MpdCommand},
        proto_client::ProtoClient, version::Version,
    },
    shared::{
        keys::ActionEvent, macros::{modal, status_error, status_info, status_warn},
        mouse_event::{MouseEvent, MouseEventKind},
        mpd_client_ext::{Enqueue, MpdClientExt},
    },
    ui::{
        UiEvent, dirstack::Dir, input::InputResultEvent,
        modals::{
            input_modal::InputModal,
            menu::{
                add_to_playlist_or_show_modal, create_delete_modal, create_save_modal,
                delete_from_playlist_or_show_confirmation, modal::MenuModal,
            },
            select_modal::SelectModal,
        },
        panes::search::inputs::{ActionResult, InputGroups, InputType, TextboxInput},
        song_list::SongListCore, widgets::browser::BrowserArea,
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
    fn items<'a>(
        &'a self,
        all: bool,
    ) -> Box<dyn Iterator<Item = (usize, &'a Song)> + 'a> {
        if all {
            Box::new(self.songs_dir.items.iter().enumerate())
        } else if !self.songs_dir.marked().is_empty() {
            Box::new(
                self
                    .songs_dir
                    .marked()
                    .iter()
                    .map(|idx| (*idx, &self.songs_dir.items[*idx])),
            )
        } else if let Some(item) = self.songs_dir.selected_with_idx() {
            Box::new(std::iter::once(item))
        } else {
            Box::new(std::iter::empty())
        }
    }
    fn enqueue(&self, all: bool) -> (Option<usize>, Vec<Enqueue>) {
        let items = self
            .items(all)
            .map(|(_, item)| Enqueue::File {
                path: item.file.clone(),
            })
            .collect_vec();
        let hovered = self.songs_dir.selected().map(|s| s.file.as_str());
        let hovered_idx = if let Some(hovered) = hovered {
            items
                .iter()
                .enumerate()
                .filter_map(|(idx, item)| {
                    if let Enqueue::File { path } = item {
                        Some((idx, path))
                    } else {
                        None
                    }
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
        let results_focused = matches!(self.phase, Phase::BrowseResults);
        let directory = &mut self.songs_dir;
        directory
            .state
            .set_content_and_viewport_len(directory.items.len(), area.height.into());
        if !directory.items.is_empty() && directory.state.get_selected().is_none() {
            directory.state.select(Some(0), 0);
        }
        let inner_block = block.inner(area);
        self.column_areas[BrowserArea::Current] = inner_block;
        self.column_areas[BrowserArea::Scrollbar] = area;
        let hover_idx = crate::ui::panes::hovered_item(
            ctx.mouse_pos(),
            inner_block,
            directory.state.inner.offset(),
            directory.items.len(),
            1,
        );
        let mut items = directory
            .to_list_items(ctx.config.theme.browser_song_format.0.as_slice(), ctx);
        if let Some(hover) = hover_idx && !directory.marked().contains(&hover) {
            items[hover] = items[hover].clone().style(config.theme.hovered_item_style);
        }
        let current = crate::ui::widgets::virtualized_list::VirtualizedList::new(items)
            .highlight_style(
                if hover_idx == directory.state.get_selected() || results_focused {
                    config.theme.hovered_item_style
                } else {
                    config.theme.current_item_style
                },
            )
            .style(config.as_list_name_style());
        frame.render_widget(block, area);
        frame
            .render_stateful_widget(
                current,
                inner_block,
                directory.state.as_render_state_ref(),
            );
        if let Some(scrollbar) = config.as_styled_scrollbar() {
            frame
                .render_stateful_widget(
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
            vec![
                Filter::new(String::new(), "")
                .with_type(FilterKind::CustomQuery(custom_query))
            ]
        } else {
            self.inputs
                .inputs
                .iter()
                .filter_map(|input| match &input {
                    InputType::Textbox(
                        TextboxInput { buffer_id, filter_key: Some(key), .. },
                    ) => {
                        let value = ctx.input.value(*buffer_id).trim().to_owned();
                        if !value.is_empty() && !key.is_empty() {
                            Some(
                                Filter::new(key.to_owned(), value)
                                    .with_type(search_mode.into()),
                            )
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
                status_error!(
                    "Rating must be a valid integer {:?}", self.inputs.rating_value(ctx)
                );
                return;
            };
            rating_filter
        } else {
            None
        };
        if filter.is_empty() && stickers_supported
            && (rating_filter.is_some() || liked_filter.is_some())
        {
            ctx.query()
                .id(SEARCH)
                .replace_id(SEARCH)
                .target(PaneType::Directories {
                    tree: TreeBrowserArgs::default(),
                })
                .query(move |client| {
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
                            ratings.retain(|uri| liked.contains(uri));
                            ratings
                        }
                        (Some(rf), None) => {
                            client
                                .find_stickers("", RATING_STICKER, Some(rf))?
                                .0
                                .into_iter()
                                .map(|s| s.file)
                                .collect()
                        }
                        (None, Some(lf)) => {
                            client
                                .find_stickers("", LIKE_STICKER, Some(lf))?
                                .0
                                .into_iter()
                                .map(|s| s.file)
                                .collect()
                        }
                        (None, None) => HashSet::new(),
                    };
                    client.send_start_cmd_list()?;
                    for uri in uris {
                        client.send_lsinfo(Some(&uri))?;
                    }
                    client.send_execute_cmd_list()?;
                    let data: Vec<Song> = client.read_response()?;
                    Ok(MpdQueryResult::SearchResult {
                        data,
                    })
                });
        } else if filter.is_empty() {
            let _ = std::mem::take(&mut self.songs_dir);
        } else {
            ctx.query()
                .id(SEARCH)
                .replace_id(SEARCH)
                .target(PaneType::Directories {
                    tree: TreeBrowserArgs::default(),
                })
                .query(move |client| {
                    let data = if fold_case {
                        client.search(&filter, strip_diacritics)
                    } else {
                        client.find(&filter)
                    }?;
                    let data = if stickers_supported && rating_filter.is_some() {
                        let ratings = client
                            .find_stickers("", RATING_STICKER, rating_filter)?;
                        let ratings: HashSet<_> = ratings
                            .into_iter()
                            .map(|r| r.file)
                            .collect();
                        data.into_iter()
                            .filter(|song| ratings.contains(&song.file))
                            .collect()
                    } else {
                        data
                    };
                    let data = if stickers_supported && liked_filter.is_some() {
                        let liked = client
                            .find_stickers("", LIKE_STICKER, liked_filter)?;
                        let liked: HashSet<_> = liked
                            .into_iter()
                            .map(|r| r.file)
                            .collect();
                        data.into_iter()
                            .filter(|song| liked.contains(&song.file))
                            .collect()
                    } else {
                        data
                    };
                    Ok(MpdQueryResult::SearchResult {
                        data,
                    })
                });
        }
    }
    fn handle_search_phase_action(
        &mut self,
        event: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let config = &ctx.config;
        if let Some(action) = event.claim_global() {
            if let GlobalAction::ExternalCommand { command, .. } = action {
                let songs = self.songs_dir.items.iter().map(|song| song.file.as_str());
                run_external(command.clone(), create_env(ctx, songs));
            } else {
                event.abandon();
            }
        }
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
                CommonAction::AddOptions { kind: AddKind::Modal(_) } => {}
                CommonAction::AddOptions { kind: AddKind::Action(opts) } if opts.all => {
                    let (_, enqueue) = self.enqueue(opts.all);
                    if !enqueue.is_empty() {
                        let current_song_idx = ctx
                            .find_current_song_in_queue()
                            .map(|(i, _)| i);
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
                CommonAction::Rate {
                    kind: _,
                    min_rating: _,
                    max_rating: _,
                    current: true,
                } => {
                    event.abandon();
                }
                CommonAction::Rate { .. } => {}
                CommonAction::Save {
                    kind: SaveKind::Playlist { name, all: true, duplicates_strategy },
                } => {
                    let song_paths: Vec<String> = self
                        .items(true)
                        .map(|(_, song)| song.file.clone())
                        .collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to save");
                        return Ok(());
                    }
                    add_to_playlist_or_show_modal(
                        name,
                        song_paths,
                        duplicates_strategy,
                        ctx,
                    );
                }
                CommonAction::Save {
                    kind: SaveKind::Modal { all: true, duplicates_strategy },
                } => {
                    let song_paths: Vec<String> = self
                        .items(true)
                        .map(|(_, song)| song.file.clone())
                        .collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to save");
                        return Ok(());
                    }
                    let modal = create_save_modal(
                        song_paths,
                        None,
                        duplicates_strategy,
                        ctx,
                    )?;
                    modal!(ctx, modal);
                }
                CommonAction::Save { .. } => {}
                CommonAction::DeleteFromPlaylist {
                    kind: DeleteKind::Playlist { name, all: true, confirmation },
                } => {
                    let song_paths: HashSet<String> = self
                        .items(true)
                        .map(|(_, song)| song.file.clone())
                        .collect();
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
                    let song_paths: HashSet<_> = self
                        .items(true)
                        .map(|(_, song)| song.file.clone())
                        .collect();
                    if song_paths.is_empty() {
                        status_warn!("No songs selected to delete");
                        return Ok(());
                    }
                    let modal = create_delete_modal(song_paths, confirmation, ctx)?;
                    modal!(ctx, modal);
                }
                CommonAction::DeleteFromPlaylist { .. } => {}
                CommonAction::LyricsNudgeUp
                | CommonAction::LyricsNudgeDown
                | CommonAction::LyricsSave
                | CommonAction::LyricsDeleteWord
                | CommonAction::LyricsEditLine
                | CommonAction::LyricsInsertBefore
                | CommonAction::LyricsInsertAfter
                | CommonAction::LyricsAddLineBefore
                | CommonAction::LyricsAddLineAfter
                | CommonAction::LyricsLineTime
                | CommonAction::LyricsSaveAndExit => {}
            }
        }
        Ok(())
    }
    fn handle_result_phase_action(
        &mut self,
        event: &mut ActionEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        let Phase::BrowseResults = &mut self.phase else {
            return Ok(());
        };
        if let Some(action) = event.claim_global() {
            match action {
                GlobalAction::ExternalCommand {
                    command,
                    ..
                } if !self.songs_dir.marked().is_empty() => {
                    let songs = self
                        .songs_dir
                        .marked_items()
                        .map(|song| song.file.as_str());
                    run_external(command.clone(), create_env(ctx, songs));
                }
                GlobalAction::ExternalCommand { command, .. } => {
                    let selected = self.songs_dir.selected().map(|s| s.file.as_str());
                    run_external(command.clone(), create_env(ctx, selected));
                }
                GlobalAction::TogglePause if self.songs_dir.marked().len() > 1 => {
                    self.open_result_phase_context_menu(ctx, None);
                }
                _ => {
                    event.abandon();
                }
            }
        }
        if let Some(action) = event.claim_directories() {
            match action {
                DirectoriesActions::FolderExpand | DirectoriesActions::PlayFile => {
                    let items = self
                        .songs_dir
                        .selected()
                        .map_or_else(
                            Vec::new,
                            |item| { vec![Enqueue::File { path : item.file.clone() }] },
                        );
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
                CommonAction::Right => {
                    let items = self
                        .songs_dir
                        .selected()
                        .map_or_else(
                            Vec::new,
                            |item| { vec![Enqueue::File { path : item.file.clone() }] },
                        );
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
                CommonAction::Confirm => {
                    self.open_result_phase_context_menu(ctx, None);
                }
                CommonAction::ContextMenu => {
                    self.open_result_phase_context_menu(ctx, None);
                }
                other => self.handle_claimed_common_action(other, event, ctx)?,
            }
        }
        Ok(())
    }
    fn open_result_phase_context_menu(
        &self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) {
        let modal = MenuModal::new(ctx)
            .anchor(anchor)
            .list_section(
                ctx,
                move |mut section| {
                    if !self.songs_dir.items.is_empty() {
                        let (_, selected_enqueue) = self.enqueue(false);
                        if !selected_enqueue.is_empty() {
                            let enqueue_clone = selected_enqueue.clone();
                            section
                                .add_item(
                                    "Add to queue",
                                    move |ctx| {
                                        ctx.command(move |client| {
                                            client.enqueue_multiple(enqueue_clone, None, None, false)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    },
                                );
                            let enqueue_clone = selected_enqueue.clone();
                            section
                                .add_item(
                                    "Replace queue",
                                    move |ctx| {
                                        ctx.command(move |client| {
                                            client.enqueue_multiple(enqueue_clone, None, None, true)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    },
                                );
                        }
                        let (_, enqueue) = self.enqueue(true);
                        if !enqueue.is_empty() {
                            let enqueue_clone = enqueue.clone();
                            section
                                .add_item(
                                    "Add all to queue",
                                    move |ctx| {
                                        ctx.command(move |client| {
                                            client.enqueue_multiple(enqueue_clone, None, None, false)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    },
                                );
                            section
                                .add_item(
                                    "Replace queue with all",
                                    move |ctx| {
                                        ctx.command(move |client| {
                                            client.enqueue_multiple(enqueue, None, None, true)?;
                                            Ok(())
                                        });
                                        Ok(())
                                    },
                                );
                            let song_files = self
                                .items(true)
                                .map(|(_, item)| item.file.clone())
                                .collect();
                            section
                                .add_item(
                                    "Create playlist from all",
                                    move |ctx| {
                                        modal!(
                                            ctx, InputModal::new(ctx).title("Create new playlist")
                                            .confirm_label("Save").input_label("Playlist name:")
                                            .on_confirm(move | ctx, value | { let value = value
                                            .to_owned(); ctx.command(move | client | { client
                                            .create_playlist(& value, song_files) ?; Ok(()) }); Ok(())
                                            })
                                        );
                                        Ok(())
                                    },
                                );
                            let song_files = self
                                .items(true)
                                .map(|(_, item)| item.file.clone())
                                .collect();
                            section
                                .add_item(
                                    "Add all to playlist",
                                    move |ctx| {
                                        let radio_playlist = ctx.config.radio.playlist.clone();
                                        let playlists = ctx
                                            .query_sync(move |client| {
                                                Ok(
                                                    client
                                                        .picker_playlists(&radio_playlist)?
                                                        .into_iter()
                                                        .map(|p| p.name)
                                                        .collect_vec(),
                                                )
                                            })?;
                                        modal!(
                                            ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                            .confirm_label("Add").title("Select a playlist")
                                            .on_confirm(move | ctx, selected, _idx | { ctx.command(move
                                            | client | { client.add_to_playlist_multiple(& selected,
                                            song_files) ?; Ok(()) }); Ok(()) }).build()
                                        );
                                        Ok(())
                                    },
                                );
                        }
                    }
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    let play_items = self.enqueue(false);
                    if !play_items.1.is_empty() {
                        section
                            .add_item(
                                "Play",
                                move |ctx| {
                                    let (hovered_song_idx, items) = play_items.clone();
                                    let current_song_idx = ctx
                                        .find_current_song_in_queue()
                                        .map(|(i, _)| i);
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
                                },
                            );
                    }
                    let song_files = self
                        .items(false)
                        .map(|(_, item)| item.file.clone())
                        .collect();
                    section
                        .add_item(
                            "Create playlist",
                            move |ctx| {
                                modal!(
                                    ctx, InputModal::new(ctx).title("Create new playlist")
                                    .confirm_label("Save").input_label("Playlist name:")
                                    .on_confirm(move | ctx, value | { let value = value
                                    .to_owned(); ctx.command(move | client | { client
                                    .create_playlist(& value, song_files) ?; Ok(()) }); Ok(())
                                    })
                                );
                                Ok(())
                            },
                        );
                    let song_files = self
                        .items(false)
                        .map(|(_, item)| item.file.clone())
                        .collect();
                    section
                        .add_item(
                            "Add to playlist",
                            move |ctx| {
                                let radio_playlist = ctx.config.radio.playlist.clone();
                                let playlists = ctx
                                    .query_sync(move |client| {
                                        Ok(
                                            client
                                                .picker_playlists(&radio_playlist)?
                                                .into_iter()
                                                .map(|p| p.name)
                                                .collect_vec(),
                                        )
                                    })?;
                                modal!(
                                    ctx, SelectModal::builder().ctx(ctx).options(playlists)
                                    .confirm_label("Add").title("Select a playlist")
                                    .on_confirm(move | ctx, selected, _idx | { ctx.command(move
                                    | client | { client.add_to_playlist_multiple(& selected,
                                    song_files) ?; Ok(()) }); Ok(()) }).build()
                                );
                                Ok(())
                            },
                        );
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |mut section| {
                    section.add_item("Cancel", |_| Ok(()));
                    Some(section)
                },
            )
            .build();
        modal!(ctx, modal);
    }
    fn scrollbar_area(&self) -> Option<Rect> {
        let area = self.column_areas[BrowserArea::Scrollbar];
        if area.width > 0 { Some(area) } else { None }
    }
    fn handle_scrollbar_interaction(
        &mut self,
        event: MouseEvent,
        ctx: &Ctx,
    ) -> Result<bool> {
        if !matches!(self.phase, Phase::BrowseResults) {
            return Ok(false);
        }
        let Some(_) = ctx.config.theme.scrollbar else {
            return Ok(false);
        };
        let Some(scrollbar_area) = self.scrollbar_area() else {
            return Ok(false);
        };
        if !matches!(
            event.kind, MouseEventKind::LeftClick | MouseEventKind::Drag { .. }
        ) {
            return Ok(false);
        }
        let content_len = self.songs_dir.items.len();
        let viewport_len = self
            .songs_dir
            .state
            .viewport_len()
            .unwrap_or(scrollbar_area.height as usize);
        let content_len = content_len
            .saturating_sub(viewport_len)
            .saturating_add(1)
            .max(1);
        let position = self.songs_dir.state.inner.offset();
        let (begin_len, end_len) = ctx.config.scrollbar_ends_width();
        if let Some(perc) = self
            .songs_dir
            .state
            .scrollbar_drag
            .handle(
                event,
                scrollbar_area,
                content_len,
                viewport_len,
                position,
                begin_len,
                end_len,
            )
        {
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
    /// The song column is the band's list area (Round 46).
    fn list_area(&self) -> Option<Rect> {
        Some(self.column_areas[BrowserArea::Current])
    }
    fn list_songs_in_item(
        &self,
        item: Song,
    ) -> impl FnOnce(
        &mut Client<'_>,
    ) -> Result<Vec<Song>> + Send + Sync + Clone + 'static {
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
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Search ");
        let inner = block.inner(search_area);
        self.inputs
            .render(inner, frame.buffer_mut(), ctx, matches!(self.phase, Phase::Search));
        frame.render_widget(block, search_area);
        self.column_areas[BrowserArea::Previous] = inner;
        self.render_song_column(frame, list_area, ctx);
        let base = ctx.config.as_list_name_style();
        let dim = ctx.config.as_list_text_style();
        let tips = vec![
            Line::from(vec![Span::styled("w/s · ↑/↓", base),
            Span::styled("  filters · results", dim),]),
            Line::from(vec![Span::styled("Enter", base),
            Span::styled("  options menu · d/→ play", dim),]),
            Line::from(vec![Span::styled("Shift+↑/↓", base),
            Span::styled("  multi-select results", dim),]),
        ];
        frame
            .render_widget(
                Paragraph::new(tips).style(dim),
                tips_area
                    .inner(Margin {
                        horizontal: 1,
                        vertical: 0,
                    }),
            );
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(ctx.config.as_border_style())
            .title(" Info ");
        let inner = block.inner(info_area);
        if let Some(song) = self.songs_dir.selected() {
            let preview = song
                .to_preview(
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
    fn on_event(
        &mut self,
        event: &mut UiEvent,
        _is_visible: bool,
        ctx: &Ctx,
    ) -> Result<()> {
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
            MouseEventKind::LeftClick if self
                .column_areas[BrowserArea::Previous]
                .contains(event.into()) => {
                self.phase = Phase::Search;
                self.inputs.focus_input_at(event.into());
                ctx.render()?;
            }
            MouseEventKind::LeftClick if self
                .column_areas[BrowserArea::Preview]
                .contains(event.into()) => {
                match self.phase {
                    Phase::Search => {
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
            MouseEventKind::LeftClick if self
                .column_areas[BrowserArea::Current]
                .contains(event.into()) => {
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
                if let Some(idx) = self.songs_dir.state.get_at_rendered_row(clicked_row)
                {
                    if event.modifiers.contains(crossterm::event::KeyModifiers::CONTROL)
                    {
                        if self.songs_dir.state.marked.is_empty() {
                            if let Some(sel) = self.songs_dir.state.get_selected() {
                                self.songs_dir.state.mark(sel);
                            }
                        }
                        self.songs_dir.state.mark(idx);
                        // Arm the band so a ctrl+drag from here adds a range.
                        self.songs_dir.state.band.arm(idx, false);
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                    } else if event
                        .modifiers
                        .contains(crossterm::event::KeyModifiers::ALT)
                    {
                        if self.songs_dir.state.mark_anchor().is_none() {
                            self.songs_dir.state.set_mark_anchor(idx);
                        }
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
                        // A plain press arms the band and defers the
                        // multi-selection drop (click ≠ drag); the release
                        // resolves it (Round 46).
                        let click_on_different_row = !self.songs_dir.marked().is_empty()
                            && Some(idx) != self.songs_dir.state.get_selected();
                        self.songs_dir.state.band.arm(idx, click_on_different_row);
                        self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                        self.songs_dir.state.set_mark_anchor(idx);
                        self.songs_dir.state.clear_range_mark();
                    }
                }
                ctx.render()?;
            }
            MouseEventKind::DoubleClick => {
                self.songs_dir.state.band.cancel();
                match self.phase {
                    Phase::Search => {
                        if self
                            .column_areas[BrowserArea::Previous]
                            .contains(event.into())
                        {
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
                }
            }
            MouseEventKind::MiddleClick if self
                .column_areas[BrowserArea::Current]
                .contains(event.into()) => {
                self.songs_dir.state.band.cancel();
                match self.phase {
                    Phase::Search => {}
                    Phase::BrowseResults => {
                        let clicked_row = event
                            .y
                            .saturating_sub(self.column_areas[BrowserArea::Current].y)
                            .into();
                        if let Some(idx) = self
                            .songs_dir
                            .state
                            .get_at_rendered_row(clicked_row)
                        {
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
            MouseEventKind::ScrollDown => {
                self.songs_dir.state.band.cancel();
                match self.phase {
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
                        self.songs_dir
                            .scroll_viewport(1, ctx.config.scroll_amount.max(1));
                        ctx.render()?;
                    }
                }
            }
            MouseEventKind::ScrollUp => {
                self.songs_dir.state.band.cancel();
                match self.phase {
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
                        self.songs_dir
                            .scroll_viewport(-1, ctx.config.scroll_amount.max(1));
                        ctx.render()?;
                    }
                }
            }
            MouseEventKind::RightClick => {
                match self.phase {
                    Phase::BrowseResults if !ctx
                        .input
                        .is_active(self.songs_dir.filter_buffer_id) => {
                        self.songs_dir.state.band.cancel();
                        let clicked_row = event
                            .y
                            .saturating_sub(self.column_areas[BrowserArea::Current].y)
                            .into();
                        if let Some(idx) = self
                            .songs_dir
                            .state
                            .get_at_rendered_row(clicked_row)
                        {
                            self.songs_dir.select_idx(idx, ctx.config.scrolloff);
                            ctx.render()?;
                        }
                        self.open_result_phase_context_menu(ctx, Some(event.into()));
                    }
                    _ => {}
                }
            }
            MouseEventKind::Drag { .. } if self.songs_dir.state.band.is_active() => {
                return SongListCore::update_band_drag(self, event, ctx);
            }
            MouseEventKind::LeftRelease if self.songs_dir.state.band.is_active() => {
                return SongListCore::finish_band_drag(self, ctx);
            }
            MouseEventKind::Drag { .. } => {}
            _ => {}
        }
        Ok(())
    }
    fn handle_insert_mode(
        &mut self,
        kind: InputResultEvent,
        ctx: &mut Ctx,
    ) -> Result<()> {
        match self.phase {
            Phase::Search => {
                match kind {
                    InputResultEvent::Push => {}
                    InputResultEvent::Pop => {}
                    InputResultEvent::Confirm => {
                        self.maybe_search_on_change(ctx);
                    }
                    InputResultEvent::NoChange => {}
                    InputResultEvent::Cancel => {
                        self.maybe_search_on_change(ctx);
                    }
                }
            }
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
