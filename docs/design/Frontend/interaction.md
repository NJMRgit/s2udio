---
title: "Mouse & Keyboard Interaction"
section: frontend
doc_type: constraints
id: "frontend/interaction"
description: >
  The shared input rules: the minimal keybinding set, wasd/arrow
  mirroring, click vs double-click semantics, right-click menus and
  back-out, wheel scrolling, modal key consumption and the mouse-over
  (hover) effects.
status: "current"
updated: "2026-08-12"
source_files:
  - src/core/input.rs
  - src/config/keys/mod.rs
  - src/shared/keys/key_resolver.rs
  - src/shared/keys/action_event.rs
  - src/ui/mod.rs
  - src/ui/modals/menu/modal.rs
  - src/ui/modals/confirm_modal.rs
  - src/ui/modals/settings.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/queue/video.rs (video-list interaction; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue/chapters.rs (chapters-list interaction; moved out of queue.rs in Phase 4b)
  - src/ui/panes/queue/context_menus.rs (Enter/right-click context menus; moved out of queue.rs in Phase 4b)
  - src/ui/panes/directories.rs
  - src/ui/panes/playlists.rs
  - src/ui/panes/radio.rs
  - src/ui/panes/search/mod.rs
  - src/shared/mouse_event.rs
  - src/ui/panes/tabs.rs
  - src/ui/panes/controls.rs
  - src/ui/panes/queue_header.rs
  - src/ui/panes/progress_bar.rs
  - src/ui/widgets/button.rs
  - src/ui/modals/menu/list_section.rs (select_section merged here in Phase 3)
  - src/ui/modals/menu/multi_action_section.rs
  - src/ui/modals/menu/input_section.rs
  - src/ui/panes/jellyfin.rs
related:
  - frontend/layout-templates
  - tabs/settings
tags: [keyboard, mouse, keybinds, interaction]
---

# Mouse & Keyboard Interaction

## The minimal keybinding set

`keybinds.clear: true` — only these defaults exist (config.ron +
`keybinds.ron` overrides):

- **global**: `Space` TogglePause, `Tab` NextTab, `E` NextTab, `Q`
  PreviousTab, `>` NextTrack, `q` Quit, `Esc` ShowSettings,
  `<S-Tab>` ToggleMpdMode (**MPD tab only**: flips its Library/Search
  mode; anywhere else it is a no-op — `Tab`/`E`/`Q` still cycle tabs,
  and the queue's `<S-Tab>` chapters toggle keeps working).
- **navigation**: `Esc` Close, `Enter` Confirm, `w`/`↑` Up, `s`/`↓` Down,
  `W`/`S`/`Shift+↑/↓` SelectUp/Down, `PageUp`/`PageDown`, `Del` Delete.
- **directories** (drives Queue, Radio, Directories and Jellyfin):
  `w`/`s` FolderUp/Down, `a` FolderCollapse, `d` FolderExpand, `→`
  PlayFile, `←` FolderCollapse.
- **queue**: `c` / `<S-Tab>` ToggleChapters (cycle the list Audio →
  Video → Chapters → Audio; Chapters only when the track has markers).

## Key resolution

- Keys resolve through a trie built from **all sections at once**; a key
  can carry multiple actions (`w` → `Common(Up)` + `Directories(FolderUp)`).
- `ActionEvent::claim_*` consumes the first action of each kind and then
  locks the event, so panes/modals that claim a common action first never
  double-fire the section action.
- Modal key flow: raw keys go to the top modal's `handle_raw_key` first;
  if unconsumed, the resolver produces an `ActionEvent` handed to the
  modal's `handle_key`. The Settings panel consumes **all** raw keys.

## wasd / arrow mirroring

- Queue, Radio, Directories and Jellyfin: `w`/`s` = `↑`/`↓`, `a` = `←`
  (collapse/back out), `d` = `→` (expand/open/play).
- **Jellyfin / MPD (Directories) right pane**: `d` does exactly what `→`
  does; `a`/`←` back out one level and collapse the branch left.
- **Radio** shares the playlists/MPD/Jellyfin scheme: one cursor on the
  list in focus — the region tree, or the station list once a region is
  entered (`d`/`→`/Enter open a region, `a`/`←` back out). `w`/`s` and
  `↑`/`↓` move that same cursor.
- Context menus: `w`/`s` + `↑`/`↓` move the highlight; **`d` and `→`
  select** the highlighted option (like Enter).

## Enter / confirm semantics

- `Enter` opens the **context menu** (the same one right-click opens) in
  the queue's Audio and Video lists and on the Playlists tab; the
  Chapters list keeps its seek behavior (`d`/`→`/Enter seek to the
  highlighted chapter).
- `d`/`→` stay the activation keys in those lists: a file plays, a
  container opens. In the browser panes (Artists/Albums, MPD, Jellyfin)
  `Enter` still plays/opens the highlighted item; on the Radio tab it
  opens a region (like `d`) or a station's context menu (like
  right-click).
- Confirmation dialogues activate on **Enter, Space, or double-click**
  (Space is consumed raw so it can't leak into the global TogglePause).
- `Esc` closes modals; with no modal open it opens Settings
  (`Close` + `ShowSettings` dual-bound — the UI checks the raw action list
  so a pane that consumed Close can't block Settings). With a
  multi-selection active the **first** Esc clears the marks and consumes
  the keypress (the settings panel only opens on a **second** Esc, when
  nothing is selected) — everywhere multi-select exists: queue audio and
  video lists, the MPD right pane, playlists, and search results.

## Mouse rules

- **Mouse-over (hover)**: the pointer position is tracked (any-event mouse
  reporting) and re-rendered, so clickable elements give hover feedback:
  - **Buttons / clickable text** (tab bar incl. Help/Settings, transport
    buttons, mode toggles, volume slider, seekbar, queue-header sort
    labels, the `● Audio ○ Video ○ Chapters` toggles, menu rows and modal
    buttons, settings sidebar + rows) render **lighter and less saturated**
    (the color blended 35% toward white; unstyled text becomes white).
    The **links in the info box** (blue `http(s)://` / `www.` URLs in the
    description body) follow the same rule: the link under the pointer
    lightens (the `hover_style` blend) — the terminal's own URL hover
    underline is disabled instead (`url_style none` in kitty.conf).
  - **List rows** (queue Audio/Video/Chapters, MPD tree + items, Playlists,
    Radio regions + stations, Jellyfin tree + items, Search results) and
    the **MPD tab's Search-mode filter inputs/spinner/button rows**
    (clickable fields) render
    with the selection highlight effect **slightly brighter** — accent ×
    0.58 vs the selection's 0.50 — but **dimmer than multi-selected
    (marked) rows** (0.65). Marked rows keep their marked highlight on
    hover.
  - Hovering the keyboard-selected row shows the hover highlight (the
    list's highlight style switches to the hover style for that row).
  - **Radio, Playlists and the MPD tab's Search mode**: the pane that holds the keyboard
    cursor renders its selection with the hover highlight even without the
    mouse, so navigation shows which pane is active — the region tree vs
    the station list (Radio `focus`), the playlists list at the root vs
    the songs pane inside a playlist (Playlists), the filter inputs vs the
    results list (Search phases). The other pane keeps the plain selection
    highlight.
  - Any **keyboard input clears the hover** (the pointer position is
    dropped): while navigating with keys nothing stays highlighted; the
    effect returns on the next pointer move (or click).
  - The pointer leaving the window (65535 convention / focus-loss)
    clears the hover.
- **Middle click**: pastes the primary selection (like a terminal without
  mouse capture) — the content goes through the same paste pipeline as
  bracketed paste / drag&drop, opening the paste popup for recognized
  audio/video content. **Ctrl+V** does the same with the clipboard proper.
- **Single click**: select/highlight only — never seeks, never plays.
  (Queue rows, chapters, directories, trees.)
- **Double click**: activate — play a track, seek a chapter, open a
  container, run a menu item.
- **Right-click**: opens the context menu in panes; with any modal open it
  **backs out** like Esc (`Modal::right_click_closes`, default true —
  Settings, Downloads and AddRandom opt out). Settings' right-click runs
  its save/discard prompt.
- **Wheel**: scrolls / moves highlights per pane — on the Queue tab it
  moves the highlight in all three lists (Audio, Video, Chapters), like
  `w`/`s`; in the settings panel it
  moves whichever pane's highlight is under the cursor.
- **Scrollbars**: click/drag targets.
- **ctrl/alt-click**: multi-select (marked rows render with the lighter
  selection style). **ctrl+click is additive**: the row under the cursor
  joins the selection too, so the initially selected item is never dropped
  and every ctrl+click only grows the marked set (clicking an
  already-marked row keeps it; a plain click on any other row clears the
  whole multi-selection). alt+click range-marks from the anchor. Available
  in the **queue's Audio and Video lists**, the **MPD pane's right pane**,
  the **Playlists tab's songs pane** and the **MPD tab's Search-mode
  results list**; `W`/`S`/`Shift+↑/↓` range-select from the click anchor in
  the same lists.
- **Ctrl+A** (`SelectAll`, navigation binding): marks every item of the
  current list — the queue's Audio/Video lists, the Playlists songs pane,
  the MPD right (Library) pane and the Search-mode results list. It does
  NOT apply to the Jellyfin/Radio/Help/Settings panes, the MPD folder
  tree, or the MPD search filter column. A second Ctrl+A keeps everything
  marked (Esc clears).
- Two-line list rows map clicks via `row / 2`.

## Contexts at a glance

| Context | Move | Activate | Close |
| --- | --- | --- | --- |
| Queue audio/video lists | w/s/↑/↓, PageUp/Down, wheel | `d`/→/double-click play; **Enter = context menu** | Esc |
| Tree (MPD/Jellyfin) | w/s/↑/↓ on the right pane | `d`/→/Enter open a folder or play a file; double-click on a tree row expands/collapses it | `a`/← |
| Tree (Radio) | w/s/↑/↓ on the focused list (regions, or stations once entered) | `d`/→/Enter open a region or play a station; Enter on a station = context menu | `a`/← |
| Playlists | w/s/↑/↓ on the current list (playlists at the root, songs inside) | `d`/→ open a playlist or play a song; **Enter = context menu**; double-click | `a`/← back out |
| Chapters | w/s/↑/↓, wheel | `d`/→/Enter / double-click (seek) | Esc |
| Context menu | w/s/↑/↓ | `d`/→/Enter / double-click | Esc / right-click |
| Confirm | ←/→ | Enter / Space / double-click | Esc |
| Settings | w/s sidebar, ↑/↓ content | `d`/Enter/Space | Esc (save/discard) |
