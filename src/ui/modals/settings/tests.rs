#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod nav_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::{
        appearance::{StagedColor, persisted_appearance, set_appearance_color},
        cava::pipewire_sources,
        model::{ContentRow, GeneralRow, SettingsFocus},
        SettingsModal,
    };
    use crate::{
        AppEvent,
        ctx::Ctx,
        shared::mouse_event::{MouseEvent, MouseEventKind},
        ui::{
            modals::{Modal, input_modal::InputModal},
            UiAppEvent,
        },
    };

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> (SettingsModal, Ctx) {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let modal = SettingsModal::new(&ctx);
        (modal, ctx)
    }

    #[test]
    fn arrows_move_content_and_left_back_to_sidebar() {
        let (mut modal, mut ctx) = fixture();
        // Open the section first (d) — the content highlight moves only
        // while the content pane owns the keyboard.
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.focus, SettingsFocus::Content);
        // Down on the general section moves past the header to "album art".
        modal.handle_raw_key(key(KeyCode::Down), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Down), &mut ctx).unwrap();
        assert_eq!(
            modal.rows[modal.selected].is_header(),
            false,
            "selection should land on a non-header row"
        );
        // Left / a: back to the sidebar (no value adjustment).
        modal.handle_raw_key(key(KeyCode::Left), &mut ctx).unwrap();
        assert_eq!(modal.focus, SettingsFocus::Sidebar);
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('a')), &mut ctx).unwrap();
        assert_eq!(modal.focus, SettingsFocus::Sidebar);
    }

    /// Open the general section and move the highlight down until `want` is
    /// selected (headers are skipped by the navigation).
    fn select_general_row(
        modal: &mut SettingsModal,
        ctx: &mut Ctx,
        want: &dyn Fn(&ContentRow) -> bool,
    ) {
        modal.handle_raw_key(key(KeyCode::Char('d')), ctx).unwrap();
        loop {
            if want(&modal.rows[modal.selected]) {
                break;
            }
            modal.handle_raw_key(key(KeyCode::Down), ctx).unwrap();
        }
    }

    #[test]
    fn space_enter_focuses_stepper_controls_and_a_d_adjust() {
        let (mut modal, mut ctx) = fixture();
        select_general_row(&mut modal, &mut ctx, &|r| {
            matches!(r, ContentRow::General(GeneralRow::Fps))
        });
        let fps_before = modal.fps();

        // Space focuses the [-] [+] controls without stepping the value.
        modal.handle_raw_key(key(KeyCode::Char(' ')), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, Some(modal.selected));
        assert_eq!(modal.fps(), fps_before, "entering adjust mode must not change the value");

        // d/→ and a/← adjust while focused.
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.fps(), fps_before + 5, "d/→ steps up");
        modal.handle_raw_key(key(KeyCode::Left), &mut ctx).unwrap();
        assert_eq!(modal.fps(), fps_before, "a/← steps back down");

        // Space/Enter commits: focus returns to the list, value kept.
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, None);
        assert_eq!(modal.fps(), fps_before);
    }

    #[test]
    fn esc_cancels_the_stepper_adjustment() {
        let (mut modal, mut ctx) = fixture();
        select_general_row(&mut modal, &mut ctx, &|r| {
            matches!(r, ContentRow::General(GeneralRow::Sensitivity))
        });
        let sens_before = modal.sensitivity();

        modal.handle_raw_key(key(KeyCode::Char(' ')), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, Some(modal.selected));
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.sensitivity(), sens_before + 10);

        // Esc cancels the whole session and reverts the staged value.
        modal.handle_raw_key(key(KeyCode::Esc), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, None);
        assert_eq!(modal.sensitivity(), sens_before, "Esc reverts the adjustment");
    }

    /// The cava device row is a stepper row: Enter/Space (and d/→) focus
    /// its [<] [>] cycle controls, a/← and d/→ walk the capture list,
    /// Space/Enter commits, Esc cancels and reverts the staged source.
    #[test]
    fn device_row_enters_adjust_mode_and_cycles_like_other_steppers() {
        let (mut modal, mut ctx) = fixture();
        select_general_row(&mut modal, &mut ctx, &|r| {
            matches!(r, ContentRow::General(GeneralRow::Device))
        });
        let source_before = modal.source();

        // Enter enters adjust mode without changing the source.
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, Some(modal.selected), "Enter focuses the device controls");
        assert_eq!(
            modal.source(),
            source_before,
            "entering adjust mode must not change the source"
        );

        // d/→ walks the capture list while focused. The list is refreshed
        // from the live PipeWire session after each step, so compute the
        // expected next source the same way the app does (with the
        // virtual-devices toggle off, only non-virtual nodes are offered).
        let expected_next = {
            let mut visible: Vec<String> = vec!["auto".to_string()];
            visible.extend(
                pipewire_sources().iter().filter(|n| !n.is_virtual).map(|n| n.name.clone()),
            );
            visible.get(1).cloned().unwrap_or_else(|| "auto".to_string())
        };
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.source(), expected_next, "d/→ steps the device forward");
        modal.handle_raw_key(key(KeyCode::Left), &mut ctx).unwrap();
        assert_eq!(modal.source(), source_before, "a/← steps the device back");

        // Space/Enter commits: focus returns to the list, value kept.
        modal.handle_raw_key(key(KeyCode::Char(' ')), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, None);
        assert_eq!(modal.source(), source_before);

        // Esc cancels the whole session and reverts the staged source. The
        // step lands on `expected_next` here too — with no non-virtual
        // PipeWire sources the capture list is only ["auto"], so stepping
        // wraps onto itself and cannot move the device.
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.source(), expected_next, "d/→ steps to the next capture source");
        modal.handle_raw_key(key(KeyCode::Esc), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, None);
        assert_eq!(modal.source(), source_before, "Esc reverts the device selection");
    }

    /// d/→ on a stepper row acts like Enter (focus the controls) instead of
    /// stepping immediately — same entry point for both keys.
    #[test]
    fn right_on_a_stepper_row_enters_adjust_mode() {
        let (mut modal, mut ctx) = fixture();
        select_general_row(&mut modal, &mut ctx, &|r| {
            matches!(r, ContentRow::General(GeneralRow::Fps))
        });
        let fps_before = modal.fps();

        // d on the row: enters adjust mode, does not step yet.
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, Some(modal.selected), "d/→ focuses the stepper controls");
        assert_eq!(modal.fps(), fps_before, "entering adjust mode must not change the value");

        // then d adjusts (and Esc still reverts).
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        assert_eq!(modal.fps(), fps_before + 5);
        modal.handle_raw_key(key(KeyCode::Esc), &mut ctx).unwrap();
        assert_eq!(modal.fps(), fps_before, "Esc reverts");
    }

    #[test]
    fn space_on_a_toggle_row_still_toggles() {
        let (mut modal, mut ctx) = fixture();
        select_general_row(&mut modal, &mut ctx, &|r| {
            matches!(r, ContentRow::General(GeneralRow::AlbumArt))
        });
        let before = modal.ui_pending.show_album_art;
        modal.handle_raw_key(key(KeyCode::Char(' ')), &mut ctx).unwrap();
        assert_eq!(modal.ui_pending.show_album_art, !before, "non-stepper rows keep toggling");
        assert_eq!(modal.adjusting, None, "toggle rows do not enter adjust mode");
    }

    #[test]
    fn scroll_wheel_moves_highlight_in_pane_under_cursor() {
        let (mut modal, mut ctx) = fixture();
        let backend = ratatui::backend::TestBackend::new(110, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|frame| modal.render(frame, &mut ctx).unwrap()).unwrap();

        // Scroll over the sidebar: the sidebar highlight moves.
        let sidebar_y = modal.sidebar_areas[0].y;
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: modal.sidebar_areas[0].x + 2,
                    y: sidebar_y,
                    kind: MouseEventKind::ScrollDown,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .unwrap();
        assert_eq!(modal.sidebar_selected, 1);

        // Scroll over the content: the content selection moves.
        let before = modal.selected;
        let content_y = modal.row_areas[0].1.y;
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: modal.row_areas[0].1.x + 10,
                    y: content_y,
                    kind: MouseEventKind::ScrollDown,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .unwrap();
        assert_ne!(modal.selected, before);
    }

    fn select_row(modal: &mut SettingsModal, pred: impl Fn(&ContentRow) -> bool) {
        modal.selected = modal.rows.iter().position(pred).unwrap();
    }

    #[test]
    fn esc_without_changes_exits_settings() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = SettingsModal::new(&ctx);
        assert!(!modal.has_changes(), "a fresh panel has no changes");

        modal.handle_raw_key(key(KeyCode::Esc), &mut ctx).unwrap();
        match app_rx.try_recv() {
            Ok(AppEvent::UiEvent(UiAppEvent::PopModal(id))) => assert_eq!(id, modal.id()),
            other => panic!("expected PopModal, got {other:?}"),
        }
    }

    #[test]
    fn esc_with_changes_prompts_save_or_discard() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = SettingsModal::new(&ctx);

        // Toggle album art off (staged): Esc must ask before closing.
        select_row(&mut modal, |r| matches!(r, ContentRow::General(GeneralRow::AlbumArt)));
        modal.activate(&mut ctx).unwrap();
        assert!(modal.has_changes());
        modal.handle_raw_key(key(KeyCode::Esc), &mut ctx).unwrap();
        // Drain render requests until the save/discard prompt shows up.
        let mut prompted = false;
        while let Ok(ev) = app_rx.try_recv() {
            if matches!(ev, AppEvent::UiEvent(UiAppEvent::Modal(_))) {
                prompted = true;
                break;
            }
        }
        assert!(prompted, "expected the save/discard prompt");
    }

    #[test]
    fn the_stream_also_downloads_toggle_is_staged_until_saved() {
        use super::super::model::{Section, TorrentRow};

        // Keep the app-event receiver alive: `activate` renders, and a
        // dropped receiver would fail the send.
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        assert!(ctx.config.torrent.stream_downloads, "the default is on");
        let mut modal = SettingsModal::new(&ctx);
        modal.section = Section::Torrent;
        modal.refresh_rows(&ctx);

        assert!(
            modal
                .rows
                .iter()
                .any(|r| matches!(r, ContentRow::Torrent(TorrentRow::StreamDownloads))),
            "the torrent section carries the toggle"
        );
        assert!(modal.torrent_stream_downloads_pending, "it opens showing the current value");

        select_row(&mut modal, |r| matches!(r, ContentRow::Torrent(TorrentRow::StreamDownloads)));
        modal.activate(&mut ctx).unwrap();
        assert!(
            !modal.torrent_stream_downloads_pending,
            "activating the row flips the staged value"
        );
        assert!(
            ctx.config.torrent.stream_downloads,
            "the live config is untouched until Save"
        );
        assert!(modal.has_changes(), "the panel now has an unsaved change");
        assert!(!modal.staged_settings().torrent_stream_downloads);
    }

    #[test]
    fn toggles_and_cava_are_staged_until_saved() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = SettingsModal::new(&ctx);
        let ui_before = ctx.config.ui;
        let fps_before = ctx.config.cava.framerate;

        // Toggle album art off.
        select_row(&mut modal, |r| matches!(r, ContentRow::General(GeneralRow::AlbumArt)));
        modal.activate(&mut ctx).unwrap();
        assert_eq!(modal.ui_pending.show_album_art, false);
        assert_eq!(ctx.config.ui, ui_before, "live ui settings must not change while staged");

        // Adjust the FPS stepper (content pane owns the keyboard).
        select_row(&mut modal, |r| matches!(r, ContentRow::General(GeneralRow::Fps)));
        modal.focus = SettingsFocus::Content;
        // d/→ on a stepper row enters adjust mode without stepping yet.
        modal.handle_raw_key(key(KeyCode::Right), &mut ctx).unwrap();
        assert_eq!(modal.adjusting, Some(modal.selected), "d/→ focuses the stepper controls");
        assert_eq!(
            modal.cava_pending.framerate, fps_before,
            "entering adjust mode must not step yet"
        );
        modal.handle_raw_key(key(KeyCode::Right), &mut ctx).unwrap();
        assert_eq!(modal.cava_pending.framerate, fps_before + 5);
        assert_eq!(
            ctx.config.cava.framerate, fps_before,
            "live cava config must not change while staged"
        );
        assert!(modal.has_changes());
    }

    #[test]
    fn remap_is_recorded_but_not_persisted() {
        let (mut modal, mut ctx) = fixture();
        let keybinds_before = ctx.config.keybinds.clone();

        // Go to the keybinds section and start capturing a new key.
        modal.handle_raw_key(key(KeyCode::Char('s')), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        select_row(&mut modal, |r| matches!(r, ContentRow::KeyItem(_)));
        modal.activate(&mut ctx).unwrap();
        assert!(modal.capturing);
        modal.handle_raw_key(key(KeyCode::Char('z')), &mut ctx).unwrap();

        assert!(!modal.pending_remaps.is_empty(), "the remap is recorded for Save");
        assert_ne!(
            ctx.config.keybinds, keybinds_before,
            "the runtime keybinds update live so the table shows the new key"
        );
        assert!(modal.has_changes());
    }

    #[test]
    fn right_click_without_changes_exits_settings() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = SettingsModal::new(&ctx);
        assert!(!modal.has_changes());
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: 5,
                    y: 5,
                    kind: MouseEventKind::RightClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .unwrap();
        match app_rx.try_recv() {
            Ok(AppEvent::UiEvent(UiAppEvent::PopModal(id))) => assert_eq!(id, modal.id()),
            other => panic!("expected PopModal, got {other:?}"),
        }
    }

    #[test]
    fn right_click_with_changes_prompts_save_or_discard() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut modal = SettingsModal::new(&ctx);
        select_row(&mut modal, |r| matches!(r, ContentRow::General(GeneralRow::AlbumArt)));
        modal.activate(&mut ctx).unwrap();
        assert!(modal.has_changes());
        modal
            .handle_mouse_event(
                MouseEvent {
                    x: 5,
                    y: 5,
                    kind: MouseEventKind::RightClick,
                    modifiers: KeyModifiers::NONE,
                },
                &mut ctx,
            )
            .unwrap();
        let mut prompted = false;
        while let Ok(ev) = app_rx.try_recv() {
            if matches!(ev, AppEvent::UiEvent(UiAppEvent::Modal(_))) {
                prompted = true;
                break;
            }
        }
        assert!(prompted, "expected the save/discard prompt");
    }

    #[test]
    fn right_click_closes_an_open_modal() {
        let (app_tx, app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, app_rx.clone()),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut ui = crate::ui::Ui::new(&ctx).unwrap();
        let modal: Box<dyn Modal + Send + Sync> =
            Box::new(InputModal::new(&ctx).title("test").on_confirm(|_ctx, _value| Ok(())));
        let id = modal.id();
        ui.on_ui_app_event(UiAppEvent::Modal(modal), &mut ctx).unwrap();

        ui.handle_mouse_event(
            MouseEvent {
                x: 5,
                y: 5,
                kind: MouseEventKind::RightClick,
                modifiers: KeyModifiers::NONE,
            },
            &mut ctx,
        )
        .unwrap();

        let mut popped = false;
        while let Ok(ev) = app_rx.try_recv() {
            if matches!(ev, AppEvent::UiEvent(UiAppEvent::PopModal(p)) if p == id) {
                popped = true;
                break;
            }
        }
        assert!(popped, "right-click should pop the top modal like Esc");
    }

    #[test]
    fn discard_restores_runtime_keybinds() {
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let mut ctx = crate::tests::fixtures::ctx(
            (app_tx, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut ui = crate::ui::Ui::new(&ctx).unwrap();
        let mut modal = SettingsModal::new(&ctx);
        let snapshot = modal.keybinds_snapshot.clone();

        // Simulate a remap applied while the panel was open.
        modal.handle_raw_key(key(KeyCode::Char('s')), &mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        select_row(&mut modal, |r| matches!(r, ContentRow::KeyItem(_)));
        modal.activate(&mut ctx).unwrap();
        modal.handle_raw_key(key(KeyCode::Char('z')), &mut ctx).unwrap();
        assert_ne!(ctx.config.keybinds, snapshot, "the remap mutated the runtime keybinds");

        ui.on_ui_app_event(UiAppEvent::DiscardSettings { keybinds: snapshot }, &mut ctx).unwrap();
        assert_eq!(
            ctx.config.keybinds, modal.keybinds_snapshot,
            "discard restores the keybinds taken when the panel opened"
        );
    }

    /// A state.ron written before the virtual-devices toggle existed still
    /// parses: the new field defaults to `false` (the old transient view
    /// filter's default) instead of failing the whole state file.
    #[test]
    fn legacy_state_ui_without_virtual_devices_field_still_parses() {
        let legacy = r#"(
            last_tab: Some("Queue"),
            mpd_library_path: None,
            video_playback: None,
            mpv_audio_lang: None,
            mpv_subtitles: None,
            mpv_svp: None,
            ui: Some((
                show_album_art: true,
                show_lyrics: true,
                show_cava: true,
                show_radio_tab: true,
                show_jellyfin_tab: true,
                auto_show_chapters: true,
            )),
            appearance: None,
        )"#;
        let state: crate::config::state::AppStateFile =
            ron::de::from_str(legacy).expect("legacy state parses");
        let ui = state.ui.expect("ui record present");
        assert!(!ui.show_virtual_devices, "missing field defaults to off");
        assert!(
            ui.mpdris2_notifications,
            "the mpdris2 notification toggle defaults to on (matches mpDris2)"
        );
    }

    /// The settings-panel UI toggles + appearance colors survive a restart:
    /// state.ron round-trips them, and the appearance values re-apply to a
    /// fresh config.
    #[test]
    fn ui_toggles_and_appearance_persist_across_restarts() {
        // UI toggles (incl. auto chapters) round-trip through state.ron.
        let mut state = crate::config::state::AppStateFile::default();
        state.ui = Some(crate::config::UiSettings {
            show_album_art: false,
            show_cava: false,
            auto_show_chapters: false,
            show_virtual_devices: true,
            ..Default::default()
        });
        let content = ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default())
            .expect("state serializes");
        let back: crate::config::state::AppStateFile =
            ron::de::from_str(&content).expect("state parses");
        let ui = back.ui.expect("ui restored");
        assert!(!ui.show_album_art);
        assert!(!ui.show_cava);
        assert!(!ui.auto_show_chapters);
        assert!(ui.show_virtual_devices, "the virtual-devices toggle round-trips");

        // Appearance colors: persist the resolved theme values and apply
        // them to a fresh config.
        let (app_tx, _app_rx) = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(
            (app_tx, _app_rx),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let mut config = ctx.config.as_ref().clone();
        // "text color" (the content text) and "UI colors" (the chrome
        // accent) are separate targets now.
        set_appearance_color(
            &mut config.theme,
            super::super::appearance::AppearanceTarget::Text,
            Some(ratatui::style::Color::Rgb(0xff, 0xb4, 0x54)),
        );
        set_appearance_color(
            &mut config.theme,
            super::super::appearance::AppearanceTarget::Ui,
            Some(ratatui::style::Color::Rgb(0x11, 0x22, 0x33)),
        );
        set_appearance_color(
            &mut config.theme,
            super::super::appearance::AppearanceTarget::Background,
            Some(ratatui::style::Color::Rgb(0xaa, 0xbb, 0xcc)),
        );
        let values = persisted_appearance(
            &config,
            None,
            &[StagedColor::Unchanged; 7],
        );
        assert_eq!(
            values[super::super::appearance::AppearanceTarget::Text as usize].as_deref(),
            Some("#ffb454")
        );
        assert_eq!(
            values[super::super::appearance::AppearanceTarget::Ui as usize].as_deref(),
            Some("#112233")
        );
        assert_eq!(
            values[super::super::appearance::AppearanceTarget::Background as usize].as_deref(),
            Some("#aabbcc")
        );

        let mut fresh = crate::config::Config::default();
        super::super::appearance::apply_persisted_appearance(&mut fresh, &values, false);
        assert_eq!(
            fresh.theme.list_text_color,
            Some(ratatui::style::Color::Rgb(0xff, 0xb4, 0x54)),
            "text color restores to the content text color"
        );
        assert_eq!(
            fresh.theme.text_color,
            Some(ratatui::style::Color::Rgb(0x11, 0x22, 0x33)),
            "UI colors restores to the chrome accent"
        );
        assert_eq!(
            fresh.theme.background_color,
            Some(ratatui::style::Color::Rgb(0xaa, 0xbb, 0xcc))
        );

        // With an auto-theme accent active, the accent-managed targets (UI
        // colors / borders / focus border / selection) are left to the
        // watcher so a stale persisted color doesn't flash at launch; the
        // content text color and background still restore.
        let default_text = crate::config::Config::default().theme.text_color;
        let mut blur = crate::config::Config::default();
        super::super::appearance::apply_persisted_appearance(&mut blur, &values, true);
        assert_eq!(
            blur.theme.text_color, default_text,
            "UI colors left to the auto-theme watcher, not the persisted value"
        );
        assert_eq!(
            blur.theme.list_text_color,
            Some(ratatui::style::Color::Rgb(0xff, 0xb4, 0x54)),
            "content text color is not accent-managed and restores"
        );
        assert_eq!(blur.theme.background_color, Some(ratatui::style::Color::Rgb(0xaa, 0xbb, 0xcc)));
    }
}

/// Round 111 follow-up: the Settings > Advanced page.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod advanced_tests {
    use super::super::{
        SettingsModal,
        model::{AdvancedRow, ContentRow, Section, SettingsFocus},
    };
    use crate::ui::modals::Modal;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }
    use crate::ctx::Ctx;

    fn fixture() -> (SettingsModal, Ctx) {
        let events = crossbeam::channel::unbounded();
        let work = crossbeam::channel::unbounded();
        let client = crossbeam::channel::unbounded();
        let ctx = crate::tests::fixtures::ctx(events, work, client);
        let modal = SettingsModal::new(&ctx);
        (modal, ctx)
    }

    /// The page exists in the sidebar and carries exactly the rows the
    /// diagnostics block promises. A missing page would be invisible to the
    /// user; an extra row would be an undocumented control.
    #[test]
    fn the_advanced_page_lists_the_diagnostics_rows() {
        let (mut modal, ctx) = fixture();
        assert!(
            Section::all().contains(&Section::Advanced),
            "Advanced must be reachable from the settings sidebar"
        );
        assert_eq!(Section::Advanced.name(), "advanced");

        modal.section = Section::Advanced;
        modal.refresh_rows(&ctx);
        let rows = modal.rows.clone();
        // ContentRow has no PartialEq (it carries non-comparable payloads), so
        // compare the debug rendering: exact rows, in order.
        assert_eq!(
            rows.iter().map(|r| format!("{r:?}")).collect::<Vec<_>>(),
            vec![
                format!("{:?}", ContentRow::Advanced(AdvancedRow::DiagnosticsHeader)),
                format!("{:?}", ContentRow::Advanced(AdvancedRow::MemoryWatchdog)),
                format!("{:?}", ContentRow::Advanced(AdvancedRow::MemoryWatchdogInterval)),
                format!("{:?}", ContentRow::Advanced(AdvancedRow::DaemonHeader)),
                format!("{:?}", ContentRow::Advanced(AdvancedRow::DaemonAutostart)),
                format!("{:?}", ContentRow::Advanced(AdvancedRow::QatHotkey)),
            ],
            "the Advanced page's rows are a contract, not an implementation detail"
        );
        // Only the headers are headers — otherwise navigation would skip the
        // rows the user came here for.
        assert!(rows[0].is_header());
        assert!(!rows[1].is_header());
        assert!(!rows[2].is_header());
        assert!(rows[3].is_header());
        assert!(!rows[4].is_header());
        assert!(!rows[5].is_header());
    }

    /// A `ui:` block that names only one toggle must still parse, filling the
    /// rest from defaults.
    ///
    /// Regression guard: without `#[serde(default)]` on `UiSettings` this
    /// fails with "missing field `show_album_art`", and `AppStateFile::load`
    /// then silently returns *all* defaults — so hand-writing one setting in
    /// `state.ron` would quietly discard every other persisted setting.
    #[test]
    fn a_partial_ui_block_in_state_ron_still_parses() {
        let partial = ron::de::from_str::<crate::config::state::AppStateFile>(
            r#"#![enable(implicit_some)]
            (
                ui: (
                    memory_watchdog: true,
                    memory_watchdog_interval_secs: 30,
                ),
            )"#,
        )
        .expect("a partial ui block must parse");

        let ui = partial.ui.expect("the ui block is read");
        assert!(ui.memory_watchdog, "the named field is applied");
        assert_eq!(ui.memory_watchdog_interval_secs, 30);
        assert!(ui.show_album_art, "unnamed fields fall back to their defaults");
    }

    /// The watchdog defaults off in both the runtime config and a freshly
    /// parsed config file, so enabling it is always a deliberate act.
    #[test]
    fn the_watchdog_defaults_off() {
        let ui = crate::config::UiSettings::default();
        assert!(!ui.memory_watchdog, "the watchdog must default off");
        assert_eq!(
            ui.memory_watchdog_interval_secs,
            crate::shared::mem_watch::DEFAULT_INTERVAL_SECS
        );
    }

    /// The interval row must be a **stepper**, so Enter/Right focuses it and
    /// Left/Right adjust it. It was missing from `is_stepper_row`, so those
    /// keys fell through to `activate`, which for this row did nothing —
    /// the reported "I can't focus it and adjust with left/right".
    #[test]
    fn the_interval_row_is_adjustable() {
        let (mut modal, mut ctx) = fixture();
        modal.section = Section::Advanced;
        modal.refresh_rows(&ctx);

        let idx = modal
            .rows
            .iter()
            .position(|r| {
                matches!(
                    r,
                    ContentRow::Advanced(AdvancedRow::MemoryWatchdogInterval)
                )
            })
            .expect("the Advanced page lists the interval row");
        assert!(
            modal.is_stepper_row(idx),
            "the interval row must be a stepper row, or Enter/Right do nothing"
        );

        // Right arrow from the content pane enters adjust mode.
        modal.focus = SettingsFocus::Content;
        modal.selected = idx;
        modal.handle_raw_key(key(KeyCode::Right), &mut ctx).unwrap();
        assert_eq!(
            modal.adjusting,
            Some(idx),
            "Right must put the interval row into adjust mode"
        );

        // Right again increases, Left decreases — both directions, which is
        // the half that reported "the value only increases".
        let start = modal.ui_pending.memory_watchdog_interval_secs;
        modal.handle_raw_key(key(KeyCode::Right), &mut ctx).unwrap();
        let up = modal.ui_pending.memory_watchdog_interval_secs;
        assert!(up > start, "Right increases the interval ({start} -> {up})");

        modal.handle_raw_key(key(KeyCode::Left), &mut ctx).unwrap();
        assert_eq!(
            modal.ui_pending.memory_watchdog_interval_secs, start,
            "Left decreases it back — the value must move both ways"
        );
    }

    /// Toggling the row stages the change rather than touching the live
    /// config: the settings panel's save/discard contract must hold for this
    /// row like every other.
    #[test]
    fn toggling_stages_without_applying() {
        let (mut modal, mut ctx) = fixture();
        let before = ctx.config.ui.memory_watchdog;
        modal.toggle_advanced(&mut ctx, AdvancedRow::MemoryWatchdog).unwrap();
        assert_ne!(
            modal.ui_pending.memory_watchdog, before,
            "the staged value flips"
        );
        assert_eq!(
            ctx.config.ui.memory_watchdog, before,
            "the live config is untouched until Save"
        );
        assert!(modal.has_changes(), "a staged toggle counts as a change");
    }

    /// The daemon-autostart row stages like every other toggle: Save is what
    /// installs/removes the systemd user unit, not the keystroke.
    #[test]
    fn daemon_autostart_is_staged_and_saved_as_a_change() {
        let (mut modal, mut ctx) = fixture();
        let initial = modal.daemon_autostart_pending;
        let system_before = crate::core::daemon::autostart::enabled();
        modal
            .toggle_advanced(&mut ctx, AdvancedRow::DaemonAutostart)
            .unwrap();
        assert_ne!(modal.daemon_autostart_pending, initial, "the staged value flips");
        assert_eq!(
            crate::core::daemon::autostart::enabled(),
            system_before,
            "toggling must not touch the installed unit"
        );
        assert!(modal.has_changes(), "a staged autostart toggle is a change");
        assert_eq!(
            modal.staged_settings().daemon_autostart,
            modal.daemon_autostart_pending,
            "Save receives the staged value"
        );
    }

    /// The QAT hotkey row stages a chord; Save persists daemon.ron and tells
    /// the running daemon. The live daemon must not be touched by the edit.
    #[test]
    fn qat_hotkey_is_staged_and_saved_as_a_change() {
        let (mut modal, _ctx) = fixture();
        let initial = modal.qat_hotkey_pending.clone();
        assert!(!modal.has_changes(), "a fresh panel has no changes");
        modal.qat_hotkey_pending = "Meta+Shift+T".to_owned();
        assert_ne!(modal.qat_hotkey_pending, initial);
        assert!(modal.has_changes(), "a staged hotkey is a change");
        assert_eq!(
            modal.staged_settings().qat_hotkey,
            "Meta+Shift+T",
            "Save receives the staged chord"
        );
    }
}

/// Auto theming (Settings -> Appearance, "auto theme"): the row that picks
/// the accent source at runtime (off / auto / sttm / matugen / kde).
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod auto_theme_tests {
    use super::super::model::ThemeFlag;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use ratatui::style::Style;

    use super::super::{
        SettingsModal,
        model::{Click, ContentRow},
    };
    use crate::{
        config::auto_theme::{AccentSource, AppliedAccent, AutoThemeSource},
        ctx::Ctx,
        ui::modals::Modal,
    };

    /// The row's rendered text (spans concatenated).
    fn row_text(modal: &mut SettingsModal, ctx: &Ctx) -> String {
        let mut targets = Vec::new();
        let line = modal.render_row(
            &ContentRow::AutoTheme,
            true,
            false,
            ctx,
            Style::default(),
            Style::default(),
            Style::default(),
            0,
            0,
            80,
            &mut targets,
        );
        let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(
            targets.iter().any(|(_, click)| *click == Click::Toggle),
            "the whole row is clickable: {targets:?}"
        );
        text
    }

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn fixture() -> (SettingsModal, Ctx) {
        let ctx = crate::tests::fixtures::ctx(
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
            (crossbeam::channel::unbounded().0, crossbeam::channel::unbounded().1),
        );
        let modal = SettingsModal::new(&ctx);
        (modal, ctx)
    }

    /// The two `[theme]` toggles are actually painted in the appearance
    /// section, with their state — the panel is where a user finds them, so a
    /// row that exists but never renders is the same as missing.
    #[test]
    fn the_theme_flag_rows_render_in_the_appearance_section() {
        let (mut modal, mut ctx) = fixture();
        let backend = ratatui::backend::TestBackend::new(110, 30);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        // Walk to the last rows so they are inside the rendered viewport.
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        for _ in 0..modal.rows.len() {
            modal.handle_raw_key(key(KeyCode::Down), &mut ctx).unwrap();
        }
        terminal.draw(|frame| modal.render(frame, &mut ctx).unwrap()).unwrap();
        let buf = terminal.backend().buffer();
        let text: String = (0..30)
            .flat_map(|y| (0..110).map(move |x| (x, y)))
            .map(|(x, y)| buf[(x, y)].symbol().to_owned())
            .collect();
        for label in ["dim behind modals", "band marquee while dragging"] {
            assert!(text.contains(label), "the row `{label}` is rendered: {text}");
        }
        assert!(text.contains("[x]"), "the rows show their state: {text}");
    }

    /// The `[theme]` booleans are ordinary staged rows: flipping one changes
    /// its pending value, marks the panel dirty and leaves the live theme
    /// alone until Save.
    #[test]
    fn the_theme_flag_rows_toggle_and_stage() {
        let (mut modal, mut ctx) = fixture();
        let before = modal.theme_flags_pending;
        // Walk to the first flag row and activate it.
        modal.handle_raw_key(key(KeyCode::Char('d')), &mut ctx).unwrap();
        while !matches!(modal.rows[modal.selected], ContentRow::ThemeFlag(ThemeFlag::ModalBackdrop)) {
            modal.handle_raw_key(key(KeyCode::Down), &mut ctx).unwrap();
        }
        assert!(!modal.has_changes(), "nothing staged yet");
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        assert_eq!(
            modal.theme_flags_pending[ThemeFlag::ModalBackdrop.index()],
            !before[ThemeFlag::ModalBackdrop.index()],
            "the row flips its own flag"
        );
        assert!(modal.has_changes(), "a flipped flag is a change to save");
        assert_eq!(
            ctx.config.theme.modal_backdrop, before[0],
            "the live theme is untouched until Save"
        );
        // Save hands the staged value to the UI.
        let staged = modal.staged_settings();
        assert_eq!(staged.theme_flags, modal.theme_flags_pending);
    }

    /// Open the general section and walk down to the auto-theme row (the last
    /// row of the appearance section).
    fn select_auto_theme_row(modal: &mut SettingsModal, ctx: &mut Ctx) {
        modal.handle_raw_key(key(KeyCode::Char('d')), ctx).unwrap();
        while !matches!(modal.rows[modal.selected], ContentRow::AutoTheme) {
            modal.handle_raw_key(key(KeyCode::Down), ctx).unwrap();
        }
    }

    #[test]
    fn the_appearance_section_lists_the_auto_theme_row() {
        let (modal, _ctx) = fixture();
        // The `[theme]` booleans close the section; auto-theme sits above them.
        assert!(
            matches!(
                modal.rows[modal.rows.len() - 2],
                ContentRow::ThemeFlag(ThemeFlag::ModalBackdrop)
            ) && matches!(
                modal.rows.last(),
                Some(ContentRow::ThemeFlag(ThemeFlag::BandRect))
            ),
            "the theme toggles close the appearance section"
        );
        // It sits below the color rows, not among them.
        let auto = modal
            .rows
            .iter()
            .position(|row| matches!(row, ContentRow::AutoTheme))
            .expect("the row exists");
        let last_color = modal
            .rows
            .iter()
            .rposition(|row| matches!(row, ContentRow::Appearance(_)))
            .expect("appearance colors exist");
        assert!(auto > last_color);
    }

    #[test]
    fn the_row_starts_on_the_configured_source() {
        let (modal, ctx) = fixture();
        assert_eq!(modal.auto_theme_pending, ctx.config.auto_theme.source);
        assert_eq!(
            ctx.config.auto_theme.source,
            AutoThemeSource::Auto,
            "auto is the default so existing blur setups keep working"
        );
    }

    #[test]
    fn cycling_stages_without_touching_the_live_config() {
        let (mut modal, mut ctx) = fixture();
        let before = ctx.config.auto_theme.source;
        select_auto_theme_row(&mut modal, &mut ctx);

        // Enter cycles forward: auto -> sttm.
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        assert_eq!(modal.auto_theme_pending, AutoThemeSource::Sttm);
        assert_eq!(
            ctx.config.auto_theme.source, before,
            "the live config is untouched until Save"
        );
        assert!(modal.has_changes(), "a staged source counts as a change");

        // Right (in the content pane) activates the row too, like Enter.
        modal.handle_raw_key(key(KeyCode::Right), &mut ctx).unwrap();
        assert_eq!(modal.auto_theme_pending, AutoThemeSource::Matugen);
        // Stepping backwards is the cycle helper the row uses (Left leaves
        // the content pane, exactly like the other option rows).
        modal.step_auto_theme(&mut ctx, -1).unwrap();
        assert_eq!(modal.auto_theme_pending, AutoThemeSource::Sttm);
    }

    #[test]
    fn the_cycle_wraps_at_both_ends() {
        let (mut modal, mut ctx) = fixture();
        select_auto_theme_row(&mut modal, &mut ctx);
        // Walk all the way round: five sources, so five steps return home.
        let start = modal.auto_theme_pending;
        for _ in 0..AutoThemeSource::ALL.len() {
            modal.step_auto_theme(&mut ctx, 1).unwrap();
        }
        assert_eq!(modal.auto_theme_pending, start);
        // Backwards from off lands on kde.
        modal.auto_theme_pending = AutoThemeSource::Off;
        modal.step_auto_theme(&mut ctx, -1).unwrap();
        assert_eq!(modal.auto_theme_pending, AutoThemeSource::Kde);
    }

    #[test]
    fn discarding_drops_the_staged_source() {
        let (mut modal, mut ctx) = fixture();
        select_auto_theme_row(&mut modal, &mut ctx);
        modal.handle_raw_key(key(KeyCode::Enter), &mut ctx).unwrap();
        assert!(modal.has_changes());
        // A fresh panel (the discard path) stages the configured value again.
        let fresh = SettingsModal::new(&ctx);
        assert_eq!(fresh.auto_theme_pending, ctx.config.auto_theme.source);
        assert!(!fresh.has_changes());
    }

    #[test]
    fn the_row_renders_every_source() {
        let (mut modal, ctx) = fixture();
        let text = row_text(&mut modal, &ctx);
        assert!(text.contains("auto theme"), "label: {text}");
        for source in AutoThemeSource::ALL {
            assert!(
                text.contains(source.as_str()),
                "the '{}' option is offered: {text}",
                source.as_str()
            );
        }
    }

    #[test]
    fn the_label_names_the_source_that_is_winning() {
        let (mut modal, mut ctx) = fixture();
        let text = row_text(&mut modal, &ctx);
        assert!(text.contains("auto theme (desktop accent)"));
        // Once a source has supplied the accent, the row says which one.
        ctx.accent.seed(
            Some(AppliedAccent {
                source: AccentSource::Matugen,
                color: ratatui::style::Color::Rgb(1, 2, 3),
            }),
            None,
        );
        assert!(row_text(&mut modal, &ctx).contains("auto theme (matugen)"));
    }

    #[test]
    fn the_source_persists_through_state_ron() {
        let mut state = crate::config::state::AppStateFile::default();
        state.auto_theme_source = Some(AutoThemeSource::Kde.as_str().to_owned());
        let content =
            ron::ser::to_string_pretty(&state, ron::ser::PrettyConfig::default()).unwrap();
        let back: crate::config::state::AppStateFile = ron::de::from_str(&content).unwrap();
        assert_eq!(
            back.auto_theme_source
                .as_deref()
                .and_then(AutoThemeSource::parse),
            Some(AutoThemeSource::Kde)
        );
        // A legacy state file without the field leaves the config default.
        let legacy: crate::config::state::AppStateFile = ron::de::from_str("()").unwrap();
        assert_eq!(legacy.auto_theme_source, None);
    }
}

/// Auto theming and the appearance colors: a settings save must persist the
/// *configured* colors, never the transient accent the watcher applied.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod appearance_baseline_tests {
    use ratatui::style::Color;

    use super::super::appearance::{
        AppearanceTarget, StagedColor, persisted_appearance, set_appearance_color,
    };

    /// No appearance row was edited in this save.
    fn untouched() -> [StagedColor; 7] {
        [StagedColor::Unchanged; 7]
    }

    /// The user staged a new UI-colors value.
    fn staged_ui(color: Color) -> [StagedColor; 7] {
        let mut staged = untouched();
        staged[AppearanceTarget::Ui as usize] = StagedColor::Set(color);
        staged
    }

    /// A config whose configured UI accent is `configured`, plus the baseline
    /// snapshot the watcher keeps, plus the live theme after an accent was
    /// applied on top of it.
    fn accent_applied(configured: Color, accent: Color) -> (crate::config::Config, crate::config::theme::UiConfig) {
        let mut config = crate::config::Config::default();
        set_appearance_color(&mut config.theme, AppearanceTarget::Ui, Some(configured));
        crate::config::derive_theme_accents(&mut config.theme);
        let baseline = config.theme.clone();
        config.theme.text_color = Some(accent);
        crate::config::derive_theme_accents(&mut config.theme);
        (config, baseline)
    }

    #[test]
    fn a_save_during_an_active_accent_keeps_the_configured_ui_accent() {
        let configured = Color::Rgb(0x11, 0x22, 0x33);
        let (config, baseline) = accent_applied(configured, Color::Rgb(0xff, 0x00, 0x00));
        let values = persisted_appearance(&config, Some(&baseline), &untouched());
        assert_eq!(
            values[AppearanceTarget::Ui as usize].as_deref(),
            Some("#112233"),
            "the configured accent persists, not the transient one"
        );
        // Without the baseline (no accent active) the live value is used.
        let values = persisted_appearance(&config, None, &untouched());
        assert_eq!(values[AppearanceTarget::Ui as usize].as_deref(), Some("#ff0000"));
    }

    #[test]
    fn an_explicit_ui_colors_edit_wins_over_the_baseline() {
        let configured = Color::Rgb(0x11, 0x22, 0x33);
        let (mut config, baseline) = accent_applied(configured, Color::Rgb(0xff, 0x00, 0x00));
        let edited = Color::Rgb(0xaa, 0x00, 0x00);
        // The save handler applies staged colors to the live theme first.
        set_appearance_color(&mut config.theme, AppearanceTarget::Ui, Some(edited));
        let values = persisted_appearance(&config, Some(&baseline), &staged_ui(edited));
        assert_eq!(values[AppearanceTarget::Ui as usize].as_deref(), Some("#aa0000"));
    }
}
