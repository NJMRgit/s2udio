# s2udio — rmpc fork

A heavily modified fork of [rmpc](https://github.com/mierak/rmpc) v0.11.0 —
a fully featured **media center TUI** for MPD: internet radio, Jellyfin
audio + video (via mpv), YouTube streams, synchronized lyrics, album art,
a visualizer and deep theming. The binary is **`s2u`** (the Cargo package
keeps the name `rmpc`). **Configs (round 23)**: everything lives in
`~/.config/s2udio/` — `config.ron`, `state.ron`/`keybinds.ron`/
`cava.ron`/`jellyfin.ron`, `themes/` and the s2udio-owned `.lrc` library
`lyrics/`. Nothing is read from or written to `~/.config/rmpc` anymore
(legacy files are migrated once on first run). Runtime caches go to
`~/.cache/s2udio/` (legacy `~/.cache/rmpc` honored for migration).

## What's different

- **Tabs** — `Queue │ Playlists │ MPD • Jellyfin • Radio • Search` (the
  library tab is named **MPD**) with right-aligned `Help | Settings`
  buttons (adaptive separators on narrow terminals). Help opens a per-tab
  keybinding popup; Esc closes it.
- **Queue** — the list area has three sub-tabs, `● Audio ○ Video ○
  Chapters` (the `c` key or `Shift+Tab` cycles them; the row sits above
  the queue box):
  **Audio** is the playback queue (local files and resolved YouTube
  streams — which show their cached title; radio/Jellyfin temp streams
  are filtered out) — `w`/`s` / `↑`/`↓` move, `d`/`→` play the
  highlighted track, `Enter` opens the right-click menu,
  `Space` play/pause, clicking the **Album** column header cycles the sort
  (album track order → a-z → z-a), ctrl/alt-click or Shift+arrows
  multi-select with a lighter marked highlight, and the context-menu
  **Remove** deletes every marked song. **Video** shows the mpv playlist
  (Title | Duration, the playing entry marked, `d`/`→` load an
  entry, `Enter` opens the right-click menu, click highlights and a
  second click loads). **Chapters** appears
  when the current track (or the mpv video) has chapter markers — the
  chapter list is keyboard-driven and clicking a chapter highlights it,
  a second click seeks to it. Playlists are created **audio-only or
  video-only** from the right-click menu: *Create audio playlist* saves
  the visible queue rows (hidden temp/stream entries never leak in), and
  the video queue's *Create video playlist* stores its URLs (hidden for
  Jellyfin sessions).
- **Radio** — a radio-browser.info browser: favourites (MPD `radio`
  playlist, capped at 10), the 100 closest stations (geo-IP or configured
  location, within 300 km), countries and states, with a disk cache
  (`~/.cache/rmpc/radio-directory.json`). Stations play through a temporary
  queue entry removed on stop/song change. Navigation is the same
  MPD/Jellyfin scheme as the other tabs: one cursor on the focused list
  (region tree, or stations once a region is entered), `d`/`→`/Enter open
  a region or play a station, `a`/`←` back out.
- **Jellyfin** — a Jellyfin server browser (same server/account as the
  `jellytui` TUI client, credentials from `~/.config/jellytui/config.toml`):
  library views → artists → albums → songs (or folders → seasons →
  episodes). Audio plays through MPD as a temporary stream; video items
  launch in **mpv** (pausing MPD while they play — it stays paused after
  the video closes; the two never play at once: music starting pauses the
  video, a resumed video pauses the music, even while the TUI is closed —
  the `s2u-mpv-tracker` daemon enforces it), play their audio through
  MPD, or ask — per the
  `video.playback` setting (Settings → general → `video:`). An **episode
  loads its whole season** into mpv as a playlist (starting at the clicked
  episode, resume/progress follow the playing entry); while a video plays
  the Queue tab's **Video** list shows the playlist, the **Chapters** tab
  shows the item's chapters, the album art shows its thumbnail and the
  lyrics/info box shows its details.
- **System media controls (MPRIS)** — the official **mpdris2-git**
  (AUR / CachyOS repos) shows MPD (s2udio re-tags streams, so the media
  controls show the real resolved title/artist of YouTube / Jellyfin
  streams), and the bundled **s2udio-mpris** daemon (auto-started by the
  mpv tracker) exposes the mpv video session (Jellyfin / YouTube / local
  video) with its title + poster — media controls follow the video too.
  Stream **album art** in the MPRIS art slot is served by the bundled
  **s2u-mpdris2** shim (step 4/8): it runs the *official* mpDris2 binary
  and extends its `find_cover()` at runtime to serve the stream thumbnail
  s2udio writes to `~/.cache/rmpc/mpris-art` (cache-busted), and hardens
  seeking (MPD cannot seek HLS streams — `Seek`/`SetPosition` no-op with
  a "Seek rejected by MPD (stream not seekable)" log instead of killing
  the daemon). All packages
  are official; the shims are version-guarded compat layers, not patched
  copies.
- **Paste / drag&drop** — middle-click pastes, Ctrl+V and files dropped on
  the terminal open a popup: **Play** (single item, without touching the
  queue), **Add to queue and play** (after the current track, starting
  playback immediately), **Append to queue**, **Add to playlist** and
  **Create Playlist** — the same five options in the `[Audio]` and
  `[Video]` sections. Works for local
  audio/video files, direct audio URLs, and
  YouTube/Soundcloud/NicoVideo links (resolved via `yt-dlp -g`, no
  download); video items follow the `video.playback` mode.
- **Directories (the `MPD` tab) / Playlists / Search** — the MPD tab is a
  jellyfin-style browser: folder tree left (`Library ↴` root, kept at a
  **min width of 50 cols** and **hidden entirely on TUIs ≤ 120 cols**
  wide), the right pane lists the current folder's children (top
  directories at the root) with directories prefixed `▶` and songs
  bare, `d`/`→` open a folder or play a file, `Enter` opens the
  right-click context menu (like the Playlists pane), `a`/`←` back out and
  collapse the branch left. The same left-pane min-width/hide behavior
  applies to the **Playlists** list and the **Jellyfin** library tree
  (shared `tree_width` helper); when the left pane is hidden, scroll
  lands on the right pane. The Playlists tab shares that navigation
  scheme (its right pane lists every playlist at the root, the songs
  inside one; `Enter` there opens the right-click menu, `d`/`→` open or
  play), prefixes each playlist `♪` (audio) or `▶` (video,
  detected from its contents) and shows stream entries with their cached
  title in dark blue instead of a raw URL. The queue's Video list, the
  MPD right pane and the Playlists songs pane also support the
  ctrl/alt-click + `W`/`S`/`Shift+↑/↓` multi-selection of the audio
  queue, and the playlists song menu can *Remove from playlist* the
  highlighted song. On the MPD and Playlists tabs the bottom **info box
  takes about two thirds of the pane height, capped at 15 lines** (on
  taller terminals the files/songs list takes the extra space). The
  Search tab is
  re-laid out to match the others: filter pane left, `Results` list,
  tips and an `Info` box right.
- **Lyrics box = lyrics/info combo** — while paused it shows the queue
  selection's details; while playing it shows the current song's details
  when no lyrics exist (or the mpv video's details while one plays), and
  the lyrics themselves when they do. The box has a clean border with no
  title; LRC files are fetched/loaded automatically.
- **Controls bar** — one info row: the channel/show/album (album for
  music, channel for YouTube, show for Jellyfin, station name for radio)
  left-aligned (truncated, never scrolls) with the Artist+Song / episode
  / movie / video title centered between it and the row's buttons (a
  carousel when it overflows), then a separator and the clickable
  transport cluster + volume slider. The buttons are the MPD mode
  toggles `Repeat Random Single Consume` while MPD plays (Single always
  oneshot — yellow when active, Consume cycles off → on → oneshot) — and
  while an mpv session is the UI source they swap for **`⤓ [Audio] [Sub]`**:
  `[Audio]`/`[Sub]` open a help-style language popup (preference + live
  re-select of the running mpv's track), `⤓` (only for a resolved ytdlp
  stream) opens the save-as menu.
- **Stream downloads** — a resolved YouTube/Soundcloud/NicoVideo stream
  can be saved into **`~/Downloads/s2udio-downloads`** (the MPD tab shows the folder at the top of the library as **`Downloads`**):
  save as audio or video, and when the media has chapters either one
  file with chapters or each chapter as its own file (named after the
  chapter title). Right-clicking a stream row (queue Audio / queue Video
  / playlists tab) adds **Download**, and the downloaded file(s)
  **replace the stream** in that list at its position.
- **Settings panel** (`Esc` with no menu open) — two-pane navigator
  (general / keybinds / mpv / mpd / jellyfin) with full mouse support.
  Rows are label-left / control-right (toggles, steppers, `[Reload]`,
  mode cycles); steppers render `value [-] [+]`. **Everything is staged**: toggles, cava + smoothing (FPS, freqs, sample
  rate/bits, channels, FIFO/Pipewire, noise reduction, monstercat, waves),
  appearance colors and key remaps only apply on **Save** — Esc with
  changes prompts Save/Discard, Esc without exits. The **mpv** section
  sets the audio/subtitle preferences as preference chains: audio
  `{system language | chosen} > original`, subtitles
  `signs > {hidden | system language | chosen}` (Enter opens a language
  picker). The **jellyfin** section signs into the server. Sidecars
  `cava.ron` / `keybinds.ron` / `state.ron` are written instead of the
  main config.
- **Blur theme watcher** — reads the active KWin blur mode
  (`~/.blur-schedule` + `~/.local/bin/blsw`) and re-derives the accent:
  pane outlines match the cava bars, the active-tab highlight matches the
  selection highlight, oneshot toggles render yellow, while content lists
  keep the theme's static text color.
- **Minimal keybind set** — global: `Space` pause, `Tab` next tab, `>`
  next track, `Q` quit, `Esc` settings; navigation: `Esc` close, `Enter`
  confirm, `w`/`s`/`↑`/`↓` move, `Shift+W/S/↑↓` select; directories/radio:
  `w`/`s` up/down, `a` collapse/back out, `d` expand/open (`d` on a bottom
  folder opens its context menu), `→` play.
  **Right-click backs out of menus** like Esc; ctrl/alt-click multi-select.
- **Mouse-over (hover) effects** — the pointer position is tracked:
  buttons/clickable text (tabs, transport, modes, volume, seekbar, queue
  header + toggles, menus, settings) lighten and desaturate on hover;
  list rows (queue Audio/Video/Chapters, MPD, Playlists, Radio, Jellyfin)
  show the selection highlight slightly brighter (accent × 0.58, between
  the 0.50 selection and the 0.65 multi-selected rows). Any keyboard
  input clears the hover until the next pointer move.

Bootstrap a config with `s2u config > ~/.config/s2udio/config.ron`.

## Design documents

The full design documentation lives in [`docs/design/`](docs/design/README.md):
**Backend** flow references (MPD playback, radio directory, Jellyfin API,
yt-dlp resolution, mpv session, paste pipeline, chapters, blur theme
watcher, image overlays, config sidecars), **Frontend** shared design
constraints (colors & typography, glyph usage, layout templates, mouse &
keyboard interaction) and **Tabs** — one spec per tab plus the Settings
panel. Every document has searchable frontmatter and cross-references its
related flows.
