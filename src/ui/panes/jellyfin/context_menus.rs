use super::*;
use super::play::jellyfin_play_temp;

impl JellyfinPane {
    pub(super) fn open_context_menu(
        &mut self,
        ctx: &Ctx,
        anchor: Option<ratatui::layout::Position>,
    ) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        if !item.is_playable() {
            return Ok(());
        }
        let name = item.name.clone();
        let is_audio_item = item.is_audio();
        let play_url = self.stream_url(&item).unwrap_or_default();
        let item_id = item.id.clone();
        let item_name = item.name.clone();
        let add_url = play_url.clone();
        let append_url = play_url.clone();
        let menu = MenuModal::new(ctx)
            .width(60)
            .title(format!(" {name} "))
            .anchor(anchor)
            .list_section(
                ctx,
                |section| {
                    let mut section = section;
                    if is_audio_item {
                        section = section
                            .item(
                                "Play now",
                                move |ctx| {
                                    ctx.query()
                                        .id(JF_PLAY)
                                        .replace_id(JF_PLAY)
                                        .target(PaneType::Jellyfin {
                                            tree: TreeBrowserArgs::default(),
                                        })
                                        .query(move |client| {
                                            let id = client.add_id(&play_url, None)?;
                                            client.play_id(id)?;
                                            Ok(MpdQueryResult::Any(Box::new(id)))
                                        });
                                    Ok(())
                                },
                            );
                    } else {
                        let mpv_url = play_url.clone();
                        let season_id = item.season_id.clone();
                        section = section
                            .item(
                                "Play with MPV (video)",
                                move |ctx| {
                                    JellyfinPane::play_video(
                                        ctx,
                                        mpv_url.clone(),
                                        item_name.clone(),
                                        item_id.clone(),
                                        season_id.clone(),
                                    )
                                },
                            );
                        section = section
                            .item(
                                "Play audio with MPD",
                                move |ctx| {
                                    jellyfin_play_temp(ctx, play_url.clone());
                                    Ok(())
                                },
                            );
                    }
                    section = section
                        .item(
                            "Add to queue",
                            move |ctx| {
                                ctx.command(move |client| {
                                    client
                                        .add(
                                            &add_url,
                                            Some(crate::mpd::QueuePosition::RelativeAdd(0)),
                                        )?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    section = section
                        .item(
                            "Append to queue",
                            move |ctx| {
                                ctx.command(move |client| {
                                    client.add(&append_url, None)?;
                                    Ok(())
                                });
                                Ok(())
                            },
                        );
                    section.add_item("Cancel", |_ctx| Ok(()));
                    Some(section)
                },
            )
            .build();
        modal!(ctx, menu);
        Ok(())
    }
    pub(super) fn search_context_menu(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.search_selected().cloned() else { return Ok(()) };
        if !item.is_playable() {
            return Ok(());
        }
        let name = item.name.clone();
        let Some(server) = self.load_server(ctx) else {
            return Ok(());
        };
        let play_url = if item.is_audio() {
            server.stream_url(&item.id)
        } else {
            server.video_stream_url(&item.id)
        };
        let item_id = item.id.clone();
        let item_name = item.name.clone();
        let is_audio_item = item.is_audio();
        let add_url = play_url.clone();
        let append_url = play_url.clone();
        let menu = MenuModal::new(ctx)
            .width(60)
            .title(format!(" {name} "))
            .list_section(
                ctx,
                |section| {
                    let mut section = section;
                    if is_audio_item {
                        section = section
                            .item(
                                "Play now",
                                move |ctx| {
                                    ctx.query()
                                        .id(JF_PLAY)
                                        .replace_id(JF_PLAY)
                                        .target(PaneType::Jellyfin {
                                            tree: TreeBrowserArgs::default(),
                                        })
                                        .query(move |client| {
                                            let id = client.add_id(&play_url, None)?;
                                            client.play_id(id)?;
                                            Ok(MpdQueryResult::Any(Box::new(id)))
                                        });
                                    Ok(())
                                },
                            );
                    } else {
                        let mpv_url = play_url.clone();
                        let season_id = item.season_id.clone();
                        section = section
                            .item(
                                "Play with MPV (video)",
                                move |ctx| {
                                    JellyfinPane::play_video(
                                        ctx,
                                        mpv_url.clone(),
                                        item_name.clone(),
                                        item_id.clone(),
                                        season_id.clone(),
                                    )
                                },
                            );
                    }
                    section = section
                        .item("Add to queue", move |ctx| {
                            let add_url = add_url.clone();
                            ctx.command(move |client| {
                                client.enqueue_multiple(
                                    vec![Enqueue::File { path: add_url }],
                                    None,
                                    None,
                                    false,
                                )?;
                                Ok(())
                            });
                            Ok(())
                        })
                        .item("Replace queue", move |ctx| {
                            let append_url = append_url.clone();
                            ctx.command(move |client| {
                                client.enqueue_multiple(
                                    vec![Enqueue::File { path: append_url }],
                                    None,
                                    None,
                                    true,
                                )?;
                                Ok(())
                            });
                            Ok(())
                        });
                    Some(section)
                },
            )
            .list_section(
                ctx,
                |section| {
                    let section = section.item("Cancel", |_ctx| Ok(()));
                    Some(section)
                },
            )
            .build();
        crate::shared::macros::modal!(ctx, menu);
        Ok(())
    }
}

