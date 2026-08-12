---
title: "Colors & Typography"
section: frontend
doc_type: constraints
id: "frontend/colors-typography"
description: >
  Shared color rules: the UI accent vs the content text color, accent
  derivation, the yellow label convention, the white/grey list palette and
  text placement rules.
status: "current"
updated: "2026-08-05"
source_files:
  - src/config/theme/mod.rs
  - src/config/mod.rs
  - src/config/theme/style.rs
related:
  - backend/blur-theme-watcher
  - frontend/layout-templates
  - tabs/settings
tags: [colors, typography, theme, accent]
---

# Colors & Typography

## The two text colors (core rule)

- **`text_color`** — the *UI chrome accent*. The blur watcher owns it
  while a mode is active; borders, cava bars, selection, seekbar and the
  controls all derive from it. Editing it = the Settings **`UI colors`**
  row.
- **`list_text_color`** — the *content text color*: the queue and the tab
  lists' secondary text. Never touched by the accent derivation or the
  blur watcher. Editing it = the Settings **`text color`** row.

`derive_theme_accents` (every config load, and after a `UI colors` edit or
blur apply):

| Target | Value |
| --- | --- |
| pane outlines (borders + focused borders) | the accent |
| selection highlight (`current_item_style.bg`) | accent × 0.50 |
| mouse-over rows (`hovered_item_style.bg`) | accent × 0.58 (between selection and marked) |
| marked (multi-selected) rows | accent × 0.65 (lighter than selection) |
| cava bars | accent |
| active-tab highlight | selection bg + accent fg |
| seekbar (elapsed/thumb) | accent; track accent × 0.4 |

`hover_color` (buttons/clickable text on mouse-over) blends the color
35% toward white — lighter **and** less saturated; text without an
explicit color renders white on hover.

## Palette conventions

- **Tab lists** (Playlists, MPD/Directories, Jellyfin, Radio, Search):
  names/rows explicit ANSI white (`as_list_name_style`), secondary text
  the configured `list_text_color` (`as_list_text_style`, never touched by
  accent derivation). The **blur accent is reserved for the selection
  highlight** (`current_item_style`).
- **Queue rows**: the `song_table_format` column styles (e.g. Album in
  white, Title/Artist/Duration in the configured grey).
- **Info boxes**: yellow group labels
  (`preview_label_style` / `preview_metadata_group_style`, untouched by
  the accent derivation). The **video info box's marquee title renders in
  explicit ANSI white** — never the auto/blur accent; its context row
  (`Channel:` / `Subs:` / `Episode:`) follows the theme color, and the
  description body uses the static `list_text_color` (the same white/
  grey rules as the tab lists). **Links** (`http(s)://` and `www.` URLs in
  the YouTube description / Jellyfin overview) are drawn in the fixed
  `LINK_BLUE` (kitty's ANSI blue `rgb(0x0d,0x73,0xcc)`, deliberately an
  RGB value so the 35%-to-white hover lightening applies — ANSI colors
  pass through `hover_color` unchanged) and the link under the pointer
  lightens on hover like any clickable text.
- **Mode toggles**: on = accent, off = dim, oneshot = yellow
  (`ControlsTheme::oneshot`).
- **Status/notice bar**: warn = yellow, error = red, info = grey.

## Typography & placement rules

- No font choices (terminal font); the constraints are **style, weight and
  alignment**:
  - Selected/highlighted rows: bold + selection background.
  - Headers (queue columns, table headers): dim.
  - Values/controls in Settings rows: bold; the label base, the
    description dim.
- Right-aligned values: video `Time:`, duration columns, appearance hex
  fields, settings controls.
- Centered: the now-playing `Artist - Title` line and the ` Settings `
  title.
- Marquee: overflowing song titles and the video info title hold ~2 s,
  scroll left, and wrap with a 5-column gap between the tail and the
  re-entering head (a continuous news-ticker).
- Text never hard-clips: long labels truncate with `…` at the display
  width (settings rows, keybind descriptions), and column values are
  padded by display width so wide glyphs can't shift rows.
