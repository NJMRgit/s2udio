#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod nav_tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    use super::super::{
        appearance::{persisted_appearance_with, set_appearance_color},
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
        let values = persisted_appearance_with(&config, false);
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

        // With a blur mode scheduled, the blur-managed targets (UI colors /
        // borders / focus border / selection) are left to the watcher so a
        // stale persisted color doesn't flash at launch; the content text
        // color and background still restore.
        let default_text = crate::config::Config::default().theme.text_color;
        let mut blur = crate::config::Config::default();
        super::super::appearance::apply_persisted_appearance(&mut blur, &values, true);
        assert_eq!(
            blur.theme.text_color, default_text,
            "UI colors left to the blur watcher, not the persisted value"
        );
        assert_eq!(
            blur.theme.list_text_color,
            Some(ratatui::style::Color::Rgb(0xff, 0xb4, 0x54)),
            "content text color is not blur-managed and restores"
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
            ],
            "the Advanced page's rows are a contract, not an implementation detail"
        );
        // Only the header is a header — otherwise navigation would skip the
        // two rows the user came here for.
        assert!(rows[0].is_header());
        assert!(!rows[1].is_header());
        assert!(!rows[2].is_header());
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
}
