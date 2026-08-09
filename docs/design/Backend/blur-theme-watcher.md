---
title: "Blur Theme Watcher"
section: backend
doc_type: flow
id: "backend/blur-theme-watcher"
description: >
  The KWin blur-mode integration: schedule reading, dominant-color
  extraction from blsw, the greyish transform, and flash-free startup.
status: "current"
updated: "2026-08-05"
source_files:
  - src/core/blur.rs
  - src/core/event_loop.rs
  - src/config/mod.rs
related:
  - backend/config-sidecars
  - frontend/colors-typography
tags: [blur, theme, accent, kwinscript]
---

# Blur Theme Watcher

## Flow overview

```
every second: BlurCheck → read ~/.blur-schedule (_MODE=<mode>)
  → mode changed? → read ~/.local/bin/blsw (never executed) for the mode's
    dominant color → greyish() → theme.text_color → derive_theme_accents
    → ConfigChanged
```

## Inputs

- `~/.blur-schedule`: a shell snippet setting `_MODE=<mode>` (e.g. a KWin
  blur theme mode name). Read every second; unreadable → retries next tick.
- `~/.local/bin/blsw`: the mode's color definitions. **Only read, never
  executed** — s2udio must never trigger blsw's KWin side effects.
  Priority: `${MODE}_OUTLINE_COLOR_ACTIVE` (RGB), then `${MODE}_TINT`
  (`#AARRGGBB`, the alpha prefix is effect opacity, not color), then
  `${MODE}_FF_LOGO_COLOR` (hex).

## The greyish transform

`greyish()` desaturates ~40 % toward the luminance grey
(`0.299r + 0.587g + 0.114b`) and lifts dark colors to a ~160 brightness
floor, so a dark mode hue stays readable on the terminal.

## Accent derivation

Sets `theme.text_color` then `derive_theme_accents`:
borders + focused borders → the accent; selection → accent × 0.50; marked
rows → accent × 0.65; cava bars, seekbar, active-tab highlight and (via
`ControlsTheme::from_ctx`) the transport buttons, mode toggles, volume
bars and the separator all follow.

## Flash-free launch

- When `~/.blur-schedule` has a mode at startup, the persisted appearance
  colors (`state.ron`) are **not** restored for the blur-managed targets
  (UI colors / Borders / FocusBorder / Selection) — the UI starts on theme
  defaults and the watcher applies the mode accent on its first tick.
- The content text color (`text color` = `list_text_color`) and the
  highlighted-item / background colors are not blur-managed and restore
  always.
- `persisted_appearance` saves the *configured* color (never the transient
  mode accent) for the blur-managed targets, so a settings save while a
  mode is active can't freeze a stale mode color into `state.ron`.

## Events

- `AppEvent::BlurCheck` — scheduled every second; fires `BlurCheck` UI
  event when the mode changed and a color was applied.
