---
title: "Config & Sidecars"
section: backend
doc_type: reference
id: "backend/config-sidecars"
description: >
  The configuration files: config.ron + themes (never rewritten), and the
  sidecars the app persists: keybinds.ron, cava.ron, state.ron,
  jellyfin.ron and the blur schedule.
status: "current"
updated: "2026-08-09 (round 23: all configs in ~/.config/s2udio)"
source_files:
  - src/config/mod.rs
  - src/config/theme/mod.rs
  - src/config/keys/mod.rs
  - src/config/cava.rs
  - src/config/state.rs
  - src/config/jellyfin.rs
  - src/config/radio.rs
  - src/config/video.rs
  - src/config/mpv.rs
related:
  - backend/blur-theme-watcher
  - tabs/settings
tags: [config, ron, sidecars, persistence]
---

# Config & Sidecars

## The rule

**`config.ron` and `themes/default.ron` are never rewritten by the app.**
User-visible changes land in small sidecar files that are merged over the
config at load time; a config watcher reloads `config.ron` on change
(sidecars are ignored by the watcher — no reload loops).

## Files

Round 23 (2026-08-09): **every s2udio config lives in
`~/.config/s2udio/`** — the single full `config.ron`, the sidecars,
`themes/` and the s2udio-owned `.lrc` library `lyrics/`. Nothing is read
from or written to `~/.config/rmpc` anymore. On first run after the
round-23 upgrade the app runs a one-time migration: the legacy
`~/.config/rmpc/config.ron` base + the round-19
`~/.config/s2udio/config.ron` overlay (if any) merge into one full
`~/.config/s2udio/config.ron`, the legacy sidecars/themes are copied, and
the legacy base is renamed `config.ron.migrated-round23`. Legacy rmpc
paths remain read-only fallbacks (sidecars, themes, caches) when the new
file is absent.

| File | Contents | Written by |
| --- | --- | --- |
| `~/.config/s2udio/config.ron` | **the full config** (address, keybinds, cava, album art, tabs, theme, radio, jellyfin, video, mpv, torrent, …) | user / `s2u config` |
| `~/.config/s2udio/themes/*.ron` | themes (designed for the extra panes/components) — resolved before any legacy rmpc dir | user |
| `~/.config/s2udio/lyrics/*.lrc` | **s2udio's own .lrc library** (round 23): fetched lyrics land here, mirrored to the library layout. The user's MPD library is checked FIRST for their own colocated `.lrc` files (read-only — never overwritten) | runtime (`rmpc-fetch-lyrics`) |
| `~/.config/s2udio/keybinds.ron` | `remove` list + per-section remap maps | Settings → keybinds (Save) |
| `~/.config/s2udio/cava.ron` | `CavaOverridesFile` | Settings → cava (Save) |
| `~/.config/s2udio/state.ron` | `AppStateFile` | runtime (tabs, settings) |
| `~/.config/s2udio/jellyfin.ron` | Jellyfin session credentials | Settings → jellyfin (Save) |
| `~/.cache/s2udio/video-playlist.json` | the Queue tab's Video list (streams / torrent files — s2udio-only, kept out of rmpc's cache) | runtime |
| `~/.cache/s2udio/mpv-mpris.json`, `mpris-art`, `yt-info.json` | mpv MPRIS bridge state + art + resolved-stream info | runtime |
| `~/.blur-schedule` | `_MODE=<mode>` | external blur scheduler |

Theme resolution order (round 23): `~/.config/s2udio/themes/{name}.ron`
→ `~/.config/s2udio/themes/{name}` → legacy
`~/.config/rmpc/themes/{name}.ron` → `~/.config/rmpc/themes/{name}` →
config dir root → raw path.

Legacy fallbacks (pre-round-23 migration): `~/.config/rmpc/state.ron`,
`keybinds.ron`, `cava.ron`, `jellyfin.ron` are still read when the
s2udio file is absent; `~/.cache/rmpc/…` cache files likewise (radio
directory, yt-info, mpris art). Writes always go to the new s2udio
locations.

## `keybinds.ron`

- `remove` list + per-section maps (`global`/`navigation`/`directories`/
  `queue`/…). Merged by `Config::apply_keybinds_override` at startup and
  on every config reload (so edits to `config.ron` keep the remaps).
- The Settings panel remap view updates the runtime keybinds live and
  writes the sidecar on Save; Discard restores the snapshot taken when
  the panel opened.

## `cava.ron` (`CavaOverridesFile`)

- Fields: `framerate`, `autosens`, `sensitivity`, `lower_cutoff_freq`,
  `higher_cutoff_freq`, `channels`, `method`, `source`,
  `noise_reduction`, `monstercat`, `waves`, `node_name`.
- **`node_name` (round 29)**: names the PipeWire node cava creates
  (`node.name`/`media.name` in pw-dump/pavucontrol/Easy Effects). cava
  hardcodes `"cava"`; s2udio renames it via the LD_PRELOAD shim built by
  `setup.sh` from `scripts/cava-node-name.c` into
  `~/.local/share/s2udio/libcavaname.so` (`S2UDIO_CAVA_NAME_SHIM`
  overrides the path) — the spawn sets `CAVA_NODE_NAME=<name>`. `None`
  (default) keeps cava's own name; `Some("")` explicitly disables
  renaming. The settings panel does not edit it but preserves it across
  Save. Only the s2udio-spawned cava carries the env, so other cava
  instances on the system keep their names.
- **No sample rate / bit depth**: a FIFO tap's format is synced from
  MPD's fifo output at spawn time (see `backend/mpd-playback`).
- Merged by `apply_cava_override`; the visualizer re-arms via
  `ConfigChanged`. Old sidecars with removed fields still parse (unknown
  fields are ignored).

## `state.ron` (`AppStateFile`)

- `last_tab`, `mpd_library_path`, `video_playback`, `mpv_audio_lang`,
  `mpv_subtitles`, the settings UI toggles (`ui`), and the appearance
  colors (`appearance`, in `AppearanceTarget::all()` order — a hex string,
  `""` = transparent, absent = theme default).
- Restored at startup; the blur watcher skips the blur-managed targets
  when a mode is scheduled (`backend/blur-theme-watcher`).

## `jellyfin.ron`

- `server_url` / `access_token` / `user_id`; takes precedence over
  jellytui's `~/.config/jellytui/config.toml`.

## Appearance persistence

`persisted_appearance` saves each target's resolved color; while a blur
mode is active the UI-accent target persists the *configured* color
(`list_text_color`) so a transient mode accent is never frozen into
`state.ron`.
