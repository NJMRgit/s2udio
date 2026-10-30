use super::*;

impl JellyfinPane {
    /// The stream URL for an item id (audio vs video endpoint).
    pub(super) fn stream_url(&self, item: &JfItem) -> Option<String> {
        self.server
            .as_ref()
            .map(|s| {
                if item.is_audio() {
                    s.stream_url(&item.id)
                } else {
                    s.video_stream_url(&item.id)
                }
            })
    }
    /// Play the given stream URL as a temporary (queue-free) entry.
    /// Play a video item, prompting when a video is already playing in mpv:
    /// a different file switches the running session to it, the same file
    /// offers to restart from the beginning. An episode's "Play with MPV"
    /// builds its whole season as the mpv playlist (starting at the clicked
    /// episode) instead of playing a single file.
    pub(super) fn play_video(
        ctx: &Ctx,
        url: String,
        name: String,
        item_id: String,
        season_id: Option<String>,
        resume_secs: Option<u64>,
    ) -> Result<()> {
        use crate::ui::modals::confirm_modal::{Action, ConfirmModal};
        if ctx.mpv.active {
            if ctx.mpv.item_id.as_deref() == Some(item_id.as_str()) {
                modal!(
                    ctx, ConfirmModal::builder().ctx(ctx)
                    .message(vec![format!("{name} is already playing."),
                    "Restart it from the beginning?".to_owned(),]).action(Action::Single
                    { on_confirm : Box::new(move | ctx | { if let Some(socket) = ctx.mpv
                    .socket.clone() { crate ::core::mpv::mpv_seek(& socket, 0.0); crate
                    ::core::mpv::mpv_unpause(& socket); } Ok(()) }), confirm_label :
                    Some("Restart"), cancel_label : Some("Cancel"), }).size((50, 6))
                    .build()
                );
            } else {
                let season_id = season_id.clone();
                modal!(
                    ctx, ConfirmModal::builder().ctx(ctx)
                    .message(vec![format!("Play {name} instead of the current video?")])
                    .action(Action::Single { on_confirm : Box::new(move | ctx | { let
                    playlist_idx = ctx.mpv.playlist.borrow().iter().position(| e | {
                    crate ::jellyfin::item_id_from_url(& e.url).is_some_and(| id | id ==
                    item_id) }); if let Some(idx) = playlist_idx && let Some(socket) =
                    ctx.mpv.socket.clone() { crate ::core::mpv::mpv_set_playlist_pos(&
                    socket, idx); } else { if let Some(socket) = ctx.mpv.socket.clone() {
                    crate ::core::mpv::mpv_loadfile(& socket, & url); } if let
                    Some(season_id) = season_id { let _ = ctx.work_sender.send(crate
                    ::shared::events::WorkRequest::FetchJellyfinSeason { season_id,
                    episode_id : item_id.clone(), },); } } let _ = ctx.app_event_sender
                    .send(crate ::AppEvent::UiEvent(crate
                    ::ui::UiAppEvent::MpvItemChanged { item_id, title : name, }),);
                    Ok(()) }), confirm_label : Some("Play"), cancel_label :
                    Some("Cancel"), }).size((50, 6)).build()
                );
            }
            return Ok(());
        }
        match ctx.config.video.playback {
            crate::config::video::VideoPlaybackMode::Mpv => {
                Self::start_video(ctx, url, name, item_id, season_id, resume_secs)?;
            }
            crate::config::video::VideoPlaybackMode::Mpd => jellyfin_play_temp(ctx, url, name),
            crate::config::video::VideoPlaybackMode::Ask => {
                let (mpv_url, mpv_name) = (url.clone(), name.clone());
                let (mpv_item, mpv_season) = (item_id.clone(), season_id.clone());
                let menu = MenuModal::new(ctx)
                    .width(60)
                    .title(format!(" {name} "))
                    .list_section(
                        ctx,
                        |section| {
                            let mut section = section;
                            section = section
                                .item(
                                    "Play with MPV (video)",
                                    move |ctx| {
                                        Self::start_video(
                                            ctx,
                                            mpv_url.clone(),
                                            mpv_name.clone(),
                                            mpv_item.clone(),
                                            mpv_season.clone(),
                                            resume_secs,
                                        )
                                    },
                                );
                            let mpd_url = url.clone();
                            let mpd_name = name.clone();
                            section = section
                                .item(
                                    "Play audio with MPD",
                                    move |ctx| {
                                        jellyfin_play_temp(ctx, mpd_url, mpd_name.clone());
                                        Ok(())
                                    },
                                );
                            section.add_item("Cancel", |_ctx| Ok(()));
                            Some(section)
                        },
                    )
                    .build();
                modal!(ctx, menu);
            }
        }
        Ok(())
    }
    /// Ask about a saved position **before** mpv is spawned, then start.
    ///
    /// The question used to be asked after the session started, on the
    /// position the app fetches from the server: mpv opened first, took the
    /// keyboard focus, and a user who was slow to answer (or never saw the
    /// prompt) ended up with the video playing behind the ask. Asking here
    /// means mpv only exists once the answer does; the choice rides
    /// `ctx.mpv.pending_resume` into the session start, which arms the seek.
    fn start_video(
        ctx: &Ctx,
        url: String,
        name: String,
        item_id: String,
        season_id: Option<String>,
        resume_secs: Option<u64>,
    ) -> Result<()> {
        use crate::ui::modals::confirm_modal::{Action, ConfirmModal};
        // The Home page asks before it leaves for the tree (it needs the
        // answer to decide *whether* to switch), so its answer is already
        // armed for this item: launch instead of asking a second time.
        let answered = ctx
            .mpv
            .pending_resume
            .borrow()
            .as_ref()
            .is_some_and(|(id, _)| id == &item_id);
        let offered = if answered {
            None
        } else {
            resume_secs
                .filter(|secs| crate::core::event_loop::should_prompt_resume(*secs as f64))
        };
        let Some(seconds) = offered else {
            Self::launch_video(ctx, &url, &name, &item_id, season_id.as_deref());
            return Ok(());
        };
        let formatted = ctx.config.duration_format.format(seconds);
        let (c_url, c_name, c_item, c_season) =
            (url.clone(), name.clone(), item_id.clone(), season_id.clone());
        let (o_url, o_name, o_item, o_season) = (url, name, item_id, season_id);
        crate::shared::macros::modal!(
            ctx,
            ConfirmModal::builder()
                .ctx(ctx)
                .message(vec![
                    format!("Resume {c_name}?"),
                    format!("Continue from {formatted}."),
                ])
                .action(Action::CustomButtons {
                    buttons: vec![
                        (
                            "Continue",
                            Box::new(move |ctx| {
                                *ctx.mpv.pending_resume.borrow_mut() = Some((
                                    c_item.clone(),
                                    crate::core::mpv::PendingResume::Continue(seconds as f64),
                                ));
                                Self::launch_video(
                                    ctx,
                                    &c_url,
                                    &c_name,
                                    &c_item,
                                    c_season.as_deref(),
                                );
                                Ok(())
                            }),
                        ),
                        (
                            "Start over",
                            Box::new(move |ctx| {
                                *ctx.mpv.pending_resume.borrow_mut() = Some((
                                    o_item.clone(),
                                    crate::core::mpv::PendingResume::StartOver,
                                ));
                                Self::launch_video(
                                    ctx,
                                    &o_url,
                                    &o_name,
                                    &o_item,
                                    o_season.as_deref(),
                                );
                                Ok(())
                            }),
                        ),
                    ],
                })
                .size((56, 7))
                .build()
        );
        Ok(())
    }
    /// Start the mpv session (or its season playlist) with no further
    /// questions. Round 64: the content name goes to mpv as the window
    /// title (it previously showed the raw stream URL).
    fn launch_video(
        ctx: &Ctx,
        url: &str,
        name: &str,
        item_id: &str,
        season_id: Option<&str>,
    ) {
        if let Some(season_id) = season_id {
            let _ = ctx.work_sender.send(
                crate::shared::events::WorkRequest::FetchJellyfinSeason {
                    season_id: season_id.to_owned(),
                    episode_id: item_id.to_owned(),
                },
            );
        } else {
            crate::core::mpv::run_mpv_titled(ctx, url, name);
        }
    }
    /// Play the highlighted item. Audio plays through MPD (temporary entry,
    /// like radio stations); video launches per the configured playback mode
    /// (mpv / MPD audio / ask).
    pub(super) fn play_selected(&mut self, ctx: &Ctx) -> Result<()> {
        let Some(item) = self.selected_item() else { return Ok(()) };
        self.play_item(&item, ctx)
    }
    /// Play one specific item (video through mpv per `video.playback`, audio
    /// through MPD as a temporary entry). The explicit target is what the
    /// tree jump plays: the cursor may have fallen back to another row, and
    /// playing the item the user asked for must not depend on that.
    pub(super) fn play_item(
        &mut self,
        item: &crate::jellyfin::JfItem,
        ctx: &Ctx,
    ) -> Result<()> {
        if !item.is_playable() {
            return Ok(());
        }
        if let Some(url) = self.stream_url(&item) {
            if item.is_audio() {
                self.play_temp_url(
                    ctx,
                    JF_PLAY,
                    PaneType::Jellyfin {
                        tree: TreeBrowserArgs::default(),
                    },
                    item.name.clone(),
                    url,
                );
                status_info!("Playing {}", item.name);
            } else {
                Self::play_video(
                    ctx,
                    url,
                    item.name.clone(),
                    item.id.clone(),
                    item.season_id.clone(),
                    item.resume_secs(),
                )?;
            }
        }
        Ok(())
    }
    pub(super) fn temp_play_id(&self) -> Option<u32> {
        self.temp_play_id
    }
    pub(super) fn set_temp_play_id(&mut self, id: Option<u32>) {
        self.temp_play_id = id;
    }
}

/// Play a Jellyfin stream URL as a temporary (queue-free) MPD entry. Used by
/// the Ask menu's "Play audio with MPD" option, where the pane itself cannot
/// be borrowed (the menu stores the closure for later).
///
/// One transactional play (`crate::shared::temp_play`): a stream MPD cannot
/// open leaves no hidden queue row behind and the status line names it.
pub(super) fn jellyfin_play_temp(ctx: &Ctx, url: String, label: String) {
    ctx.query()
        .id(JF_PLAY)
        .replace_id(JF_PLAY)
        .target(PaneType::Jellyfin {
            tree: TreeBrowserArgs::default(),
        })
        .query(move |client| {
            let id = crate::shared::temp_play::add_and_play(client, &url, &label)?;
            Ok(MpdQueryResult::Any(Box::new(id)))
        });
}
