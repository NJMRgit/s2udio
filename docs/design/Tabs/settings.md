---
title: "Settings Panel"
section: tabs
doc_type: spec
id: "tabs/settings"
description: >
  The full-window settings panel: layout, sections, row types, staging
  and save/discard behavior.
status: "current"
updated: "2026-08-05"
source_files:
  - src/ui/modals/settings.rs
  - src/ui/modals/remap_keys.rs
  - src/ui/modals/confirm_modal.rs
related:
  - backend/config-sidecars
  - backend/blur-theme-watcher
  - frontend/interaction
  - frontend/layout-templates
tags: [settings, staging, panel]
---

# Settings Panel

## Layout

A **full-window** view (no popup over the TUI): rounded border + centered
` Settings ` title; left sidebar (general / keybinds / mpv / mpd /
jellyfin, `│` divider) with the active section marked `>`; content rows on
the right; footer at the bottom. The panel consumes **all** raw keys
(`handle_raw_key` returns true always; `handle_key` is a no-op).

Content rows are a two-column table: **label left, control right-aligned**
(toggles, steppers, `[Reload]`, mode cycles, `[edit]` fields). Section
headers (`[ features ]`, `[ cava ]`, `[ appearance ]`, …) carry their
description right-aligned. Long labels shrink with `…` on narrow
terminals so controls are never cut off.

## Keyboard

- `w`/`s` move the sidebar highlight; `d` (or a sidebar click) populates
  the right pane.
- `↑`/`↓` move the content highlight (stopping at section boundaries).
- `←`/`→` adjust the highlighted option; `Enter`/`Space` toggle/pick.
- `Esc` closes (or cancels keybind capture / color edit).

## Mouse

- Wheel moves the highlight in whichever pane the cursor is over.
- Sidebar click = select + populate; row click = select + activate.
- `[-]`/`[+]`/`[<]`/`[>]` buttons adjust (button rects win over the
  row-wide target); double-click activates.

## Sections

- **general**: features block (show album art / lyrics / cava / radio tab
  / jellyfin tab toggles, `reload radio stations` `[Reload]`, `Jellyfin
  media playback preference ask | mpv | mpd`, `If media contains chapters
  open to chapters list`) + cava block (auto-sens, sensitivity 1-500 step
  5, frame rate 15-120, min/max sampling frequency, channels, method
  FIFO/Pipewire, noise reduction, monstercat, waves — sample rate / bit
  depth are gone, a FIFO tap syncs from MPD) + appearance block.
- **keybinds**: inline remap table (Description | Action | Key); select an
  action, press the new key (Esc cancels); runtime keybinds update live,
  persisted on Save.
- **mpv**: audio/subtitle preference chains (Enter opens a language
  picker).
- **mpd**: library location (directory picker), update/rescan actions,
  playback modes, outputs modal — acts immediately.
- **jellyfin**: server URL / username / password + sign in (persists the
  `jellyfin.ron` sidecar).

## Appearance rows

`label ▢▢ [hex]` — name, color swatch (background-filled; `▢▢` =
transparent), `[#hex]` field right-aligned. Targets: **text color**
(content text, `list_text_color`), **UI colors** (chrome accent,
`text_color`), border color, focused border, selection highlight,
highlighted item, background color. A UI-colors edit re-derives the
accents on Save (`backend/blur-theme-watcher`).

## Staging

- **Everything is staged**: toggles, cava values, appearance colors and
  key remaps touch the live UI only when the panel is closed with Save.
- `Esc` with no changes exits straight away; with pending changes it pops
  a Save/Discard confirm (Enter/Space/double-click to confirm).
- Save applies + persists `cava.ron` / `keybinds.ron` and writes
  `state.ron`; Discard restores the keybind snapshot taken when the panel
  opened. The mpd section acts immediately.
- Toggling cava off drops its layout row and stops the bars; the cava
  overlay never draws under the panel (`backend/image-overlays`).
