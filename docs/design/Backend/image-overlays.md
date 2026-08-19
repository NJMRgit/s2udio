---
title: "Image & Terminal-side Overlays"
section: backend
doc_type: flow
id: "backend/image-overlays"
description: >
  How the terminal-side overlays (album art, Jellyfin poster, cava bars,
  mpv MPRIS poster) are detected, rendered outside the ratatui buffer, and
  kept from painting over the UI.
status: "current"
updated: "2026-08-05"
source_files:
  - src/ui/image/facade.rs
  - src/ui/image/mod.rs
  - src/ui/panes/album_art.rs
  - src/ui/panes/cava.rs
  - src/ui/panes/jellyfin.rs
  - src/ui/mod.rs
related:
  - backend/mpv-session
  - frontend/layout-templates
tags: [overlay, kitty, sixel, album-art, cava]
---

# Image & Terminal-side Overlays

## What is an overlay

Album art, the Jellyfin poster, the cava bars and the mpv MPRIS poster draw
**outside the ratatui buffer** (kitty/iTerm2/sixel escape sequences or
direct writes). The buffer only carries placeholder cells, so ordering and
staleness rules matter.

## Backend detection

`autodetect_image_backend`: kitty → iterm2 → sixel for supported
terminals, with ueberzug/block as fallbacks. Konsole skips the preferred
set entirely; zellij disables images; a requested-but-unsupported
ueberzug falls back to block.

## Rendering order (critical)

- Overlays are displayed **after the frame's buffer flush**
  (`Ui::flush_album_art` / `flush_pending_overlays`): drawing during the
  render lets the flush overwrite the placeholder cells.
- The album art facade re-places the last drawn image **only when that
  frame's diff actually rewrote cells inside the art pane** (resize,
  layout change, modal close) — an unconditional re-place after every
  frame made the art strobe/pulse during playback (MPD status updates
  render many frames a second).
- The re-place is also skipped when the last encode no longer matches the
  current pane size; a stale-sized encode is dropped and re-encoded
  instead of drawn.
- Re-encodes happen only when the area changes; `ImageResized` forces a
  render so the flush always runs.

## Hiding rules

- Overlays are hidden while **any modal is open** (they would paint over
  the modal's full-window view): the album art and cava pause+clear on
  `ModalOpened` and restore on `ModalClosed`.
- Hidden during **window resizes** (between the first `Resized` event and
  the 500 ms debounce the window is kept blank) and while the terminal is
  too small.
- The Jellyfin poster hides on selection change / modal / resize; the
  cava row is dropped on the Jellyfin tab and while a video plays (a full
  window refresh is requested so stale bars never linger).

## Album art sources

Audio: embedded art → cover search → default image. YouTube audio stream:
the video's **thumbnail**. mpv video: Jellyfin primary image or resolved
thumbnail, else the generic default — never the paused song's audio cover.
The art pane is video-owned while mpv is active; it re-evaluates its
source on session start/end and tab entry.

## Cava specifics

The bars are drawn by a dedicated thread that reads cava's raw output and
writes only **changed columns** (bounded terminal write volume). The
thread receives Start/Stop/Pause/ConfigChanged commands; `run()` is a
no-op while a modal is open. cava is PipeWire-only (round 30) — the old
MPD-fifo sample-format sync is gone (`backend/mpd-playback`).

With the PipeWire input, cava's stream node is named **`cava`** (cava
hardcodes `pw_stream_new_simple(..., "cava", ...)` — `node.name` and
`media.name`). Round 29: when `cava.ron` sets `node_name`, s2udio spawns
cava through the `libcavaname.so` LD_PRELOAD shim (built by `setup.sh`
from `scripts/cava-node-name.c`) with `CAVA_NODE_NAME=<name>`, so the
node shows up as e.g. **`s2udio-cava`** in `pw-dump` / `pw-cli ls Node` /
pavucontrol / Easy Effects while other cava instances keep their own
names. See `backend/config-sidecars` for the config surface.
