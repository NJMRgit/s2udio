# s2udio — Handoff

A personal fork of [rmpc](https://github.com/mierak/rmpc) v0.11.0 — a
media center TUI (MPD, internet radio, Jellyfin audio/video via mpv,
YouTube streams, lyrics, album art, visualizer).

- **Repo**: `~/Projects/s2udio` (branch `master`, origin
  `https://github.com/NJMRgit/s2udio.git`; fork of upstream `dbd3b21`).
- **App**: binary **`s2u`**, installed as `~/.local/bin/s2udio`. Terminal:
  kitty (~490 px / 70 cols). **Configs (round 23)**: everything in
  `~/.config/s2udio/` — `config.ron`, sidecars, `themes/`, and the
  s2udio-owned `.lrc` library `lyrics/`. Nothing in `~/.config/rmpc`
  anymore (legacy files migrated once on first run; legacy paths are
  read-only fallbacks). Caches: `~/.cache/s2udio/` (legacy
  `~/.cache/rmpc` honored).
- **Install after every change**: build + install + restart (Build loop
  below); **restart s2udio to pick up changes**.

## The spec lives in docs/design/

This file is the **operational handoff** (state, gotchas,
housekeeping); the spec lives in the per-subsystem design docs (start at
[`docs/design/README.md`](docs/design/README.md)). Map:

| Topic | Doc |
| --- | --- |
| Queue tab (Audio/Video/Chapters, merged box, album sort, app-open) | `docs/design/Tabs/queue-tab.md` |
| Settings panel (layout, sections, staging, appearance) | `docs/design/Tabs/settings.md` |
| MPD / Radio / Jellyfin / Playlists / Search tabs | `docs/design/Tabs/*` |
| MPD playback, temp entries, MPRIS tagging, cava PipeWire capture | `docs/design/Backend/mpd-playback.md` |
| mpv session: poll, playlist, reattach, preference chains, tracker | `docs/design/Backend/mpv-session.md` |
| Radio directory: endpoints, caching, lazy loads | `docs/design/Backend/radio-directory.md` |
| Jellyfin API: endpoints, `JfItem` parsing, progress reporting | `docs/design/Backend/jellyfin-api.md` |
| yt-dlp resolution + yt-info cache lifecycle | `docs/design/Backend/ytdlp-resolution.md` |
| Stream downloads (`s2udio-downloads`, save-as, playlist replace) | `docs/design/Backend/stream-downloads.md` |
| Torrent streaming (rqbit engine, magnet/.torrent, bandwidth gate) | `docs/design/Backend/torrent-streaming.md` |
| Paste / drag&drop pipeline | `docs/design/Backend/paste-pipeline.md` |
| Chapters: sources, keying, seek routing, view rules | `docs/design/Backend/chapters.md` |
| Blur theme watcher | `docs/design/Backend/blur-theme-watcher.md` |
| Image / terminal-side overlays (art, poster, cava, MPRIS) | `docs/design/Backend/image-overlays.md` |
| Config sidecars (`keybinds.ron` / `cava.ron` / `state.ron` / `jellyfin.ron`) + rmpc/s2udio split | `docs/design/Backend/config-sidecars.md` |
| Colors & typography; one-cell glyphs; layout templates; mouse/keyboard | `docs/design/Frontend/*` |
| Validation plans (MPRIS & art: timeline/art/track-info per source) | `docs/design/Validation/mpris-validation.md` |
| Session work logs (what happened, decisions, closing state per session) | `docs/design/Sessions/` |

## User design decisions (authoritative)

The user is the authority on design; where s2udio deviates from upstream,
the user's decisions win. Locked in across sessions (details in the docs):

- Tabs `Queue │ Playlists │ MPD • Jellyfin • Radio • Search` (queue
  first; library renamed `MPD`).
- **Minimal keybind set** (`keybinds.clear: true`) — table in
  `docs/design/Frontend/interaction.md`. wasd mirrors arrows everywhere;
  in Jellyfin `d` = `→` exactly.
- Jellyfin **shared-selection navigation** (right pane = current node's
  children, highlight follows the cursor, moving up collapses the branch
  you left, no `/` root row); the **MPD tab shares the model** (`Library ↴`
  root, `▶` dirs / bare songs; left folder-tree pane **min width 50
  chars, hidden entirely when the TUI is ≤ 120 chars wide**).
- Queue: `d`/`→` play; **Enter = context menu** (right-click
  equivalent) in Audio/Video lists + Playlists tab + MPD tab (Chapters keeps
  Enter = seek); `c` **or `Shift+Tab`** cycle Audio / Video / Chapters;
  wheel moves the highlight. Chapters: single click **highlights only**,
  double click seeks.
- **Controls bar (3 rows)**: row 1 carries the **channel/show/album**
  (album for music, channel for YouTube, show for Jellyfin, station name
  for radio) **left-aligned and truncated (never scrolls)** with the
  **Artist+Song / episode/movie / video title centered between it and
  the buttons** (carousel inside that region); separator row; transport
  row. While MPD plays the row-0 buttons are `Repeat Random Single
  Consume`; while an mpv session is the UI source they are swapped for
  the mpv buttons: **⤓** (Download, only for a resolved ytdlp stream,
  furthest left) / **[Audio]** / **[Sub]** (furthest right).
  [Audio]/[Sub] open a help-style language popup and re-select the live
  mpv track; ⤓ offers save-as audio/video, and per-chapter files when
  the media has chapters.
- **Stream downloads** land in `~/Downloads/s2udio-downloads` (outside
  the MPD library — MPD cannot play them, so the MPD tab lists the
  folder from disk and Downloads files play via mpv). The MPD tab shows
  it at the top of the library (right under `Library ↴`) as
  `Downloads`. Right-clicking a stream row (queue Audio /
  queue Video / playlists tab) adds **Download**, and the downloaded
  file(s) replace the stream in that list (queue entry deleted + re-added
  at its position, video-playlist entry swapped, stored-playlist entry
  replaced at its index).
- Video info box: marquee title + bold right-aligned `Time:`, context row
  in theme color, `Description ↴` body emoji-scrubbed + scrollable;
  `http(s)://` and `www.` links are drawn blue (kitty's ANSI blue,
  `LINK_BLUE` in `lyrics.rs`) and the link under the pointer lightens on
  hover — kitty's own squiggly hover underline is disabled (`url_style
  none` in kitty.conf).
- Tab lists use the queue's white/grey palette — the blur accent is
  reserved for the selection highlight.
- **Multi-select** (ctrl/alt-click + `W`/`S`/`Shift+↑/↓`) works in the
  queue Video list, MPD right pane, Playlists songs pane.
- **Mouse-over (hover)**: buttons/clickable text lighten + desaturate
  on hover (blend toward white); list rows get the selection highlight
  slightly brighter (accent × 0.58) but **dimmer than multi-selected
  rows** (0.65); **any keyboard input clears the hover** until the next
  pointer move (details in `frontend/interaction.md`).
- **mpv reattach restores the playlist from mpv itself when the state
  file is stale/missing** (a lost playlist blanks the YouTube
  info/chapters/thumbnail, keyed by the playing URL); the tracker daemon
  is spawned on reattach too — see `core/mpv.rs` `read_mpv_playlist` and
  `docs/design/Backend/mpv-session.md`.
- Playlists tab: MPD/Jellyfin navigation scheme; right pane lists every
  playlist at the root (titled ` Playlists `), songs inside one. Radio
  tab: same scheme (region tree → stations; top-100 per region).
- Blur/STTM: the accent recolors only structure (borders, cava,
  selection, active tab); launch is flash-free.
- `video.playback` (`ask`/`mpv`/`mpd`) governs **Jellyfin video items
  only**; other video is always an explicit popup choice. mpv preference
  chains: audio `system|chosen > original`; subtitles
  `signs > hidden|system|chosen`.
- Closing mpv does **not** auto-resume MPD — it stays paused until the
  user presses play.

## Current state (verified)

- **Master**: `NJMRgit/s2udio` **master @ `fe9f078`** (clean buildable
  subset) — rounds 28 + 28b + **round 29 merged 2026-08-12** on top of
  `898a5bb` (`06b90f7` cava pipewire restart fix, `96fc4cb` round 28,
  `2c04bcf` round 28b, `fe9f078` round 29 cava node_name); validated
  **1364/1364**, warnings 3 baseline. Round 30 (cava PipeWire-only) is
  committed on `working` and not yet merged to master.
- **Branch**: `working` (tracks `s2udio-working/working`), at `5802ef1` +
  **round 30 committed on top** (host-implemented; **1364/1364**
  host-validated 2026-08-12, warnings 3 baseline, binary build clean).
  **Round 30 (2026-08-12, host-implemented)**: remove the cava FIFO input —
  cava is **PipeWire-only**. The `CavaInputMethod` enum and the settings
  panel's FIFO/PipeWire method toggle are gone (generated config always
  writes `method = pipewire`); the MPD-fifo sample-format sync
  (`paste::mpd_fifo_format`) is deleted; `setup.sh` and the container
  deploy scripts no longer append a fifo `audio_output` to mpd.conf; the
  gates G1/G4/G9 no longer require the fifo. Old configs (`method: Fifo`,
  a fifo `source` path) still parse — the method is ignored and the source
  falls back to `auto`. See
  [FEEDBACK-2026-08-12-3.md](FEEDBACK-2026-08-12-3.md).
  **Round 29 (2026-08-12, host-implemented)**: name the cava PipeWire
  node — new `node_name` option (`cava.ron` / main config; preserved by
  the settings panel) + LD_PRELOAD shim (`scripts/cava-node-name.c`,
  built by setup.sh → `~/.local/share/s2udio/libcavaname.so`,
  `S2UDIO_CAVA_NAME_SHIM` overrides) that injects node.name/media.name
  from `CAVA_NODE_NAME`; `spawn_cava` sets the env when configured.
  Live-verified: s2udio's cava is now **`s2udio-cava`** in pw-dump.
  See [FEEDBACK-2026-08-12-2.md](FEEDBACK-2026-08-12-2.md).
  **Round 28b (2026-08-12, host-implemented)**: ONLY `Shift+Tab`
  toggles the MPD tab's Library/Search mode (new global `ToggleMpdMode`,
  bound to `<S-Tab>` in the defaults + the live config.ron +
  example_config.ron; the MPD pane claims it while focused) — the
  round-28 `Tab` (NextTab) toggle claim is removed, so `Tab`/`E`/`Q`
  cycle tabs again and the MPD tab is always reachable from the
  keyboard; elsewhere `ToggleMpdMode` is a no-op (the queue's `Shift+Tab`
  chapters toggle is untouched). See
  [FEEDBACK-2026-08-12-1.md](FEEDBACK-2026-08-12-1.md).
  **Round 28 (2026-08-12, isodev implementing)**: the Search tab folded
  into the MPD tab — default tabs drop the Search entry (a leftover
  "Search" config tab is hidden from the bar/cycle via `is_tab_hidden`);
  the MPD tab gained a `⭘ Library  ● Search` toggle row (● = active;
  mouse-clickable labels; **round 28b: `Shift+Tab` toggles**, `Tab`/`E`/
  `Q` cycle tabs); Library mode = the unchanged MPD browser; Search mode
  = the folded-in search UI (rounds 24–27 parity intact, still queries
  the MPD library), session-lifetime state, Library at startup. Search
  queries now target the Directories pane and the MPD pane forwards the
  results to its embedded search. **1360/1360** (+7 tests: 6 MPD-tab + 1
  leftover-Search-tab bar) + round-28b's 2 tests, warnings 3 baseline.
  See [FEEDBACK-2026-08-12-0.md](FEEDBACK-2026-08-12-0.md).
  **Round 24
  (2026-08-10, isodev implementing)**: Esc-deselect everywhere
  multi-select exists (queue video list + MPD tab Close arms + `MarkState::
  clear_anchor`, 6 new pane tests) + search tab full
  selection/hover/dual-pane parity (ctrl/alt/plain-click, marked + hover
  render, focused-pane convention, 5 new tests). See
  [FEEDBACK-2026-08-10-17.md](FEEDBACK-2026-08-10-17.md).
  **Round 23 (2026-08-09, user feedback)**: ALL configs move
  to `~/.config/s2udio/` (base `config.ron` + sidecars + `themes/`;
  nothing in `~/.config/rmpc`) — one-time migration merges the legacy
  base + round-19 overlay and renames the legacy base aside; the lyrics
  directory becomes `~/.config/s2udio/lyrics` (s2udio's own .lrc
  library; the user's MPD library is checked FIRST, read-only — user
  .lrc files are never overwritten; s2udio-written .lrc files were
  migrated from the library). See the round-23 session entry.
  **Round 22 (2026-08-09, isodev)**: picker title stray "t" →
  **Round 22 (2026-08-09, isodev)**: picker title stray "t" →
  `▶ files — <name>` + 1-char right margin before the scrollbar
  (`paste.rs` title, `torrent_file_picker.rs` list-block right
  padding). **1280/1280** (+1 round-22 test), warnings 3 baseline.
  See [FEEDBACK-2026-08-09-16.md](FEEDBACK-2026-08-09-16.md).
  **Round 18 CLOSED host-side (2026-08-09)**: container implementation
  host-validated and 4 live-check bugs fixed (add timeout, per-engine
  listen port, picker scan capture, `--disable-persistence` +
  `overwrite=true` + wait-for-`live`) — **1267/1267**, binary
  `e2fdcc08`, user live-confirmed. **Round 19 (2026-08-09): config
  separation** — rmpc base config stays in `~/.config/rmpc/`, s2udio
  feature configs move to `~/.config/s2udio/` (overlay `config.ron` +
  `state.ron`/`keybinds.ron`/`cava.ron`/`jellyfin.ron` + `themes/`),
  stream/video playlists move to `~/.cache/s2udio/`; legacy paths still
  read (migration). **1268/1268**, binary `e2fdcc08`. See
  [FEEDBACK-2026-08-09-14.md](FEEDBACK-2026-08-09-14.md).
  **Round 20 (2026-08-09, container, uncommitted)**: torrent popup wait
  window trimmed to `Loading <label>… mm:ss` + `esc to cancel` (no
  DL-speed ✓✗ row), picker buttons **Play / Download & Play / Cancel**
  (Download & Play = multi-file "Play and Download"), duplicate-paste
  fix — canonical infohash scan keys + `Arc`-shared engines + landed
  scans kept after popup close (repeat paste reuses the engine, no
  second rqbit on the same cache dir). **1273/1273**. See
  [FEEDBACK-2026-08-09-15.md](FEEDBACK-2026-08-09-15.md).
  **Session log**:
  `docs/design/Sessions/2026-08-09.md` (round 20 container entry) and
  `docs/design/Sessions/2026-08-08.md` (round 18 host entry + round 19
  entry; the no-deadline
  scan wait window; earlier round-17/16 M2 entries, incl. the round-16
  M2 entry: torrent
  classification + `[Torrent]` popup + end-to-end Play (stream),
  container-validated 1232/1232; earlier round-15 M1 entries).
  Earlier log:
  `docs/design/Sessions/2026-08-07.md` (host round-13 entry: lyrics
  frame layout — buttons moved to the bottom row, blank top row,
  bottom `─` margin line — host-implemented from live-check feedback,
  binary `7cf770e2`; host round-12-closing entry: live-check fixes —
  hover excludes the leading space, held-⭘ survives the fetch
  completion — 1208/1208, binary `d4fd6af9`; container round-12
  entry: lyrics buttons live-check refinements — text-only hover,
  held-⭘ on release-reporting terminals, hide/show labels +
  one-cell right margin + hidden state shows the paused-style info
  panel; container round-11
  entry: lyrics buttons visual/UX refinements — no Artist-Title header,
  ⭘ pressed-while-held only, `● hide lyrics | ● fetch lyrics` cluster
  with `|` separator, `● hide | ● fetch` collapse, hover =
  queue-list highlight on the label text only; host round-11 entry:
  1205/1205, binary `f9d3532e`, live-checked —
  findings → round 12; round-10 entry: lyrics-panel header buttons —
  `hide lyrics` + `fetch lyrics`; earlier round-8/9 entries: shared
  `tree_width` left-pane min-width/hide, MPD/Playlists info boxes ≈
  2/3 height, `.min(15)` info-box cap; host validation round-9 entry:
  1196/1196, binary `8cd0da1e`).
  Earlier log
  (lyrics/karaoke + rmpc install history):
  `~/HANDOFF.md` — its paths say `~/Projects/rmpc`; the repo is now
  `~/Projects/s2udio`.
- **Tests**: `cargo test --release` → **1282/1282 passing**
  (round 23: +2 over the round-22 1280/1280 — the round-23
  `lyrics_lookup_prefers_user_library_then_s2udio_library_then_index`
  lookup-order test and the
  `migration_merges_legacy_base_and_overlay_into_s2udio_config`
  one-time-migration test. Round 22: +1 over the round-21/picker-UX 1279/1279 —
  `torrent_file_picker_round22_title_marker_and_right_margin`, which
  renders the `▶ files — <name>` title and asserts the one blank
  column before the scrollbar on a long row; the 11 picker tests'
  title argument updated to `"▶ files — Fake Pack "`. Round 20: +5
  over the round-19 host-validated 1268/1268 — canonical
  magnet scan-key test, landed-scan reuse, in-flight reuse, picker
  button labels, picker Download & Play download flag — and the two
  round-18 wait-window tests reworked for the trimmed window; binary
  warnings 3 = baseline; validated in the container with the installed
  toolchain 2026-08-09, see session log 2026-08-09.
  Installed binary md5 `846f47b3` (M2 +
  Play and Download build; the running instance picks it up on
  restart; current round-18 build installed md5 `36d58c60`, no live
  instance running).
  restart). Earlier baseline,
  rounds 12–13 closed host-side: the round-12 container changes —
  hide/show labels, text-only hover, the one-cell right margin, the
  hidden-info-panel render, `held_press_keeps_the_marker_across_`
  `renders` + `release_reporting_terminals_skip_the_fallback` — plus
  the live-check fixes: hover excludes the leading space, the held
  marker survives the fetch completion
  (`fetch_press_keeps_the_marker_across_the_fetch_completion`), and
  the round-13 layout tests: buttons on the bottom row, blank top
  row, full-width bottom margin line). Round-11 closed
  host-side: 3 compile fixes, the split_once
  space-consumption rendering bug — buttons now render
  `● hide lyrics` — and 2 style-assertion fixes in the round-11
  tests. Round-9 closed host-side:
  1194 + 2 new `info_box_height_cap` tests for the
  MPD/Playlists info-box 15-line cap; round-8 was 1190 + 4 width-regime
  tests; host fixed 6 issues that round —
  `multi_select::render_pane` 60→160 (missed
  in-container), `right_pane_lists_playlists_at_the_root` x≥1,
  `items_render_play_for_dirs_and_no_markers_for_songs` 60×16,
  width-test channel receivers kept alive — see session log). Tracker
  integration
  `python3 tests/tracker/test_tracker.py` → **6/6**; s2u-mpdris
  `python3 s2u-mpdris/tests/test_s2u_mpdris.py` → **21/21**; mpdris2-shim
  `tests/mpdris_shim/` → **1/1**.
- **Controls bar** (see `frontend/layout-templates.md` + `backend/stream-downloads.md`):
  3 rows — row 1 carries the channel/show/album (left, truncated, never
  scrolls) **and** the title (centered between it and the buttons,
  carousel when it overflows); separator; transport. MPD mode toggles
  `Repeat Random Single Consume` while MPD plays, mpv buttons
  `⤓ [Audio] [Sub]` while an mpv session is the UI source — [Audio]/[Sub]
  open a help-style language popup and re-select the live mpv track.
  Stream downloads land in `~/Downloads/s2udio-downloads` (the MPD tab
  shows it as `Downloads` at the top, listed from disk — MPD cannot
  play files there, so Downloads files play via mpv); right-click
  Download on stream rows replaces the stream in the video queue (mpv),
  while MPD queue / stored-playlist replacements keep the stream and
  report the save. Playlist. **Browser panes** (MPD, Playlists, Jellyfin): left pane min
  50 cols via shared `tree_width()`, hidden entirely on TUIs ≤ 120
  cols — when hidden, scroll lands on the right pane. **MPD and
  Playlists info boxes are ≈ 2/3 of the pane height, capped at 15
  lines (round 9 — host-validated, live check pending user
  restart).** Installed binary
  md5 `f9d3532e` (round-11 build: lyrics buttons visual/UX
  refinements; the MPRIS duration-fallback feature is in the same
  line of builds — when mpv reports `duration` unavailable for a
  DASH/HLS stream the state file falls back to the playing entry's
  known duration, so the media controls keep the timeline + seeking
  for YouTube video; tracker caretaker got the same fallback).
  **MPRIS fully live-verified 2026-08-07**: MPD audio (local + streams),
  Jellyfin video, YouTube video (timeline/seek/art/title), mpDris2
  source-following, cache-busted poster.
  scripts
  `s2u-mpv-tracker`/`s2u-mpdris2`/`s2udio-mpris` updated,
  `mpDris2.service` restarted (fixed shim — seek hardened + `find_cover`
  `_orig` late-binding fix landed in repo 2026-08-07, installed == repo
  `bc2bb231`).
- **s2u-yt client strategy (2026-08-07, live-verified)**: resolution tries
  **anonymous
  `android_vr`** first — DASH video up to 2160p, and single-file opus/m4a
  audio that MPD range-seeks (verified live: scratch-MPD seek 45 → 0:45 on
  the 95-min mix; the HLS "Not seekable" regression is gone at the source;
  **user live-check 2026-08-07: streaming + seeking confirmed working**).
  Falls back to **authenticated `web_safari`** (HLS ≤1080p) via a two-phase
  wrapper: phase 1 strips the user config's cookie options and passes
  `--ignore-config` (yt-dlp skips android_vr when the session is
  authenticated; the default config would leak cookies back in); phase 2
  retries with cookies. bgutil must run (android_vr's GVS policy requires a
  PO token — systemd user unit, `status.sh` checks). `status.sh` reports
  which client won. Installer/status `-v` probes now use `--get-id` (a bare
  `-v` call downloaded the live test stream — the recurring `s2u-yt/CSGO*.mp4`
  artifact).
- **mpv session** (see `backend/mpv-session.md`): 100 ms IPC poll;
  mpv<->MPD mutual exclusion both directions, TUI open AND closed; the
  UI follows the active source (`mpv_is_ui_source`); mpv + tracker are
  `setsid`'d and ignore SIGHUP (closing the TUI never kills the video).
  **While a video session is alive the tracker stops `mpDris2.service`
  (the paused MPD audio disappears from the media controls) and restarts
  it when mpv exits.** The `s2udio-mpris` bridge must serve every
  property through `Properties.Get` (incl. Position/Volume — a missing
  key is a None-typed reply KDE can't parse). Tracker versioned at
  `scripts/s2u-mpv-tracker` (setup.sh installs it; installed == repo).
- **Paste popup** (see `backend/paste-pipeline.md`): `Play` /
  `Add to queue and play` / `Append to queue` / `Add to playlist` /
  `Create Playlist` in both `[Audio]` and `[Video]` sections — Add
  inserts after the current track and starts playback immediately.
  **Ctrl+V** pastes the clipboard, **middle click** the primary
  selection (both via `wl-paste`/`xclip`/`xsel`); drag&drop arrives as
  bracketed paste. **Verified working** after the latest restart
  (middle-click registered; it keys off the **primary selection**, so
  non-media content is ignored by design — and `wl-paste` is the only
  reader on this box, `xclip`/`xsel` are not installed).
- **Playlists** (see `tabs/playlists-tab.md`): audio/video-only creation,
  `♪`/`▶` prefixes, cached stream titles. **Radio playlist exclusive**
  (see `tabs/radio-tab.md`): hidden from every picker. **YouTube streams
  are queue content** (see `backend/ytdlp-resolution.md`): visible queue
  rows with cached titles; one video-style info layout everywhere.
- Live config: `status_update_interval_ms: 16` (60fps status updates);
  cava framerate 90; `state.ron`: `video_playback: ask`,
  `mpv_audio_lang: custom:en`, `mpv_subtitles: hidden`.
- **Mouse-over (hover) effects** (see `frontend/interaction.md` +
  `frontend/colors-typography.md`): the pointer position is tracked and
  re-rendered; buttons/clickable text lighten + desaturate on hover
  (35% toward white), list rows (queue Audio/Video/Chapters, MPD,
  Playlists, Radio, Jellyfin) show the selection highlight at accent ×
  0.58 (between the 0.50 selection and the 0.65 marked rows); any
  keyboard input clears the hover until the next pointer move. In the
  Radio and Playlists tabs the **focused pane's cursor renders with the
  hover highlight** during keyboard navigation.
- Git: on `master`, fork of upstream `dbd3b21`, pushed to origin
  (NJMRgit/s2udio). **Check `git status`** — the working tree carries
  uncommitted session work (see Pending).

## Build / run / debug loop

- `cargo build --release` then install `target/release/s2u` →
  `~/.local/bin/s2udio` (atomic rename if "Text file busy":
  `cp new /tmp/s2u.new && mv /tmp/s2u.new ~/.local/bin/s2udio`).
- **`cargo test --release` does NOT rebuild `target/release/s2u`** —
  build BEFORE installing, or you ship a stale binary (bit us before).
- Verify a running binary: `md5sum /proc/PID/exe ~/.local/bin/s2udio`.
- The live instance runs in the user's terminal — coordinate restarts
  with the user (terminal injection is flaky).

## Repo layout (2026-08-09 restructure)

The full dev tree lives in its OWN PRIVATE repo:
**`NJMRgit/s2udio-working`** (branch `working` — this repo, everything
incl. docs + agent files). **`NJMRgit/s2udio`** holds only **`master`** —
the clean buildable subset: every push to master omits `docs/`, all agent
`.md` files (`FEEDBACK-*.md`, `HANDOFF.md`, `notes.md`), `.github/`,
`.vscode/`, `.typos.toml`, `tests/` and anything not required to
build/run the TUI. `assets/example_config.ron` + `assets/default.jpg` are
compiled in — never delete. Old history is preserved at the local
`pre-restructure` tag.

## UI reuse rewrite (branch `rewrite` — COMPLETE 2026-08-11)

The **UI reuse rewrite** (`docs/design/Rewrite/ui-reuse-rewrite.md`, branch
`rewrite` of `NJMRgit/s2udio-working`) consolidates `src/ui` around master
modules + args. Working on `rewrite` only; the distribution repo `master`
is untouched. **User priority (2026-08-10):** the rewrite's aim is extensibility +
predictable behavior; LOC reduction is a proxy, not a gate (phases may
close LOC-neutral/positive when the shared-core cost buys
one-implementation-by-construction — Phase 2 +51, Phase 3 −55).
**Rust toolchain is now available in the container** (rustup
1.97.1 installed 2026-08-10, `~/.cargo/bin`, `cargo test --release` runs
in-container — see the `RUSTUP_HOME`/`CARGO_HOME` env note below) — the
agent can self-validate; the host still does live checks.

Status (2026-08-11):
- **Phase 0 ✅** (`d0d3a56`): baseline LOC + similarity metrics
  (`scripts/dev/ui-metrics.py` — token-sequence difflib ratio over
  comment-stripped fn bodies; thin-adapter names excluded from the
  guardrail), 1312/1312 tests green, outline §2.4 baseline table.
- **Phase 1 ✅** (`cd103ac` + `cd10c75` + `113d7e7`): `SongListCore<T, S>`
  shared list core extracted (`src/ui/song_list.rs`); `BrowserPane` is a
  thin dir-stack adapter (1041 → 232 LOC); queue Audio + search results
  adopt the core and delegate all non-specific `CommonAction` arms
  (queue −208, search −236 LOC; src/ui net −179). 1312/1312 green.
- **Phase 2 ✅** (`f5c2ac4` + `948c85c` + `26e9834`): `TreeBrowserCore`
  (trait with hooks, `src/ui/tree_browser.rs`) unifies directories /
  jellyfin / radio — the shared tree+items mechanics now exist once:
  `render_tree_browser`/`render_tree`/`render_items`/`render_tips`,
  `move_tree`/`move_items`, tree+items mouse routing, the common action
  arms, the temp-play lifecycle (`cleanup_temp_play`/`temp_play_on_stop`/
  `drop_temp_play`/`play_temp_url`/`handle_play_result`), split/layout
  hooks. All three panes implement the trait (thin accessors + pane
  hooks) and delegate `render`/`handle_action`/`handle_mouse_event`/
  `on_event` to the shared defaults; radio keeps its focus-based
  `handle_action`/mouse/back_out. 1312/1312 green (directories 21,
  jellyfin 15, radio 31). Metrics: the 9 heavy tree-family pairs + the
  jellyfin↔radio `on_event` temp-play pair are gone (live in the core);
  residual >0.5 pairs are thin Pane-delegator shells (same category the
  baseline already carried) + pre-existing non-tree pairs. Net LOC:
  src/ui +51 vs the Phase-0 table (panes −369: directories −187,
  jellyfin −192, radio +10; core +598) — trait-with-hooks trades raw LOC
  for one-implementation-by-construction, same tradeoff as Phase 1; the
  args/config end-state is Phase 6. **Behavior deltas — RESOLVED by
  Phase 2.1** (2026-08-10): (1) jellyfin/radio items-box titles keep the
  space before the count again (`" Items (3) "`, `" Stations (3) "` —
  `items_title` is now the pre-padded title, the shared format is
  `"{}({}) "`; one render test per pane pins the exact title); (2) the
  unified temp-play Stop cleanup is kept and pinned (directories Stop
  drops the temp entry, jellyfin Stop clears `ctx.temp_play_id`, radio
  `PLAY` sets `ctx.temp_play_id` — one regression test each);
  (3) 1318/1318 green, warnings 3 baseline, guardrail unchanged at 60.
- **Phase 3 ✅** (`a5aac04` + `53b9e90`): modal consolidation onto the
  two master modules + the section merge. `ListModal<'a, V>`
  (`src/ui/modals/list_modal.rs`) implements the options-list +
  scrollbar (+ drag) + `ButtonGroup` + List↔Buttons focus cycle +
  Confirm/Close/wheel/click/double-click handling once, parameterized
  by args (`row_fn`/`size_fn`, `buttons`/`confirm_buttons`,
  `multi_select`+`mark_id`, `bottom_title`, `list_right_padding`,
  `wheel_moves_selection`, `scrollbar_drag`); `SelectModal` (317 → 90)
  and `TorrentFilePicker` (501 → 141) are thin adapters with unchanged
  public builders (all 15 call sites + the paste picker tests
  untouched). `menu/select_section.rs` merged into `list_section.rs`
  (select items = `MenuItem` + section-level `action` callback);
  `SectionType::Select` + its 16 dispatch arms deleted; the
  `select_section` builder method is gone (3 call sites use
  `list_section` + `add_select_item`). `InfoListModal` generalized to
  N columns + a `header` arg and absorbs `DecodersModal` (248 → 67);
  `zip_longest2` (dead) removed from `shared/ext.rs`. **1326/1326**
  green (+8: 4 ListModal + 4 InfoListModal behavior pins incl. the
  unified click-row mapping — the legacy SelectModal's extra `-1` is
  fixed, see outline §5.2), warnings 3 baseline, guardrail unchanged at
  60. Net LOC: **src/ui −61** (56,923 → 56,862), tree −110 — short of the
  −600–900 target (same tradeoff as Phase 2's +51: masters front-load the
  shared core; consumer paydown is Phase 6). Kept
  standalone (not list-shaped, rationale in outline §5.2):
  `OutputsModal`, `InfoModal`, `DownloadsModal`, `LanguageModal`,
  `TabHelpModal`, `AddRandomModal`.
- **Phase 4a ✅** (`9b46f54`): the `● Audio ○ Video ○ Chapters` toggle row
  is now the reusable `SubTabBar` widget (`src/ui/widgets/sub_tab_bar.rs`,
  rewrite §4.4); queue's `render_toggle_on_border` delegates, public API
  unchanged, 1328/1328 (2 new widget tests).
- **Phase 4b ✅** (`5bf5a18` + `80b4844` + `c81cb2f` + 4b4 close-out,
  2026-08-11): queue.rs decomposed into the module root +
  `queue/context_menus.rs` (347) + `queue/video.rs` (373) +
  `queue/chapters.rs` (325) — 4447 → **3485** (production 2673 → 1711,
  **−962**), zero test edits (1775 test LOC byte-identical), 1328/1328,
  warnings 3 baseline, guardrail unchanged at 60 excl-thin (identical
  pair set). Moved fns are `pub(super)` inherent methods on `QueuePane`
  (private-in-child-module visibility); `seek_to` moved with chapters,
  `current_chapters`/`chapters_available` stayed; queue.rs stays a FILE.
  Docs: outline §2.4/phase-row/§5.3, HANDOFF, plan flipped done, session
  log. **Next: 4c/5 — phase-4 host live-check (plan §8) then Phase 5
  (shared drawing widgets from controls/lyrics).**
- **Phase 5 ✅** (`483a73c` + `490c62e` + `2fcb10c` + `ef4863f` + 5b5
  close-out, 2026-08-11): shared drawing widgets extracted from
  controls/lyrics per `docs/design/Rewrite/phase5-drawing-widgets.md` —
  **marquee** (`ui/widgets/marquee.rs`, 282 LOC: `draw_marquee`/
  `marquee_offset`/`draw_panel_at` + the `CAROUSEL_*` constants, cycle
  math untouched; the 2 marquee timing tests moved with the code) adopted
  by controls (2 render sites) + lyrics + jellyfin; **wrap**
  (`ui/widgets/wrap.rs`, 84 LOC: `wrap_to_width`/`wrap_spans`) adopted by
  lyrics + jellyfin (jellyfin's `lyrics::wrap_to_width` import flipped).
  Controls.rs 1516 → 1319, lyrics.rs 2543 → 2475, src/ui +103, 1328/1328
  after every commit, warnings 3 baseline, guardrail 60 excl-thin
  identical pair set. Button cluster + now-playing templates:
  **documented decisions NOT to merge** (§3 — three cluster shapes / two
  template shapes; rationale in outline §5.4 + plan §3a). `ScrollingLine`
  kept (different continuous-`|`-wrap cycle). **Next: 6 — args expansion
  (pane-specific constants into `PaneType`/config args); phase-4/5 host
  live-checks pending.**
- **Phase 6 ✅** (`4a5b054` + `a1caf6b` + `9abb201` + 6.4 close-out,
  2026-08-11): pane-specific browser constants moved into `PaneType`/
  config args per `docs/design/Rewrite/phase6-args-expansion.md` —
  `TreeBrowserArgs { tree_min_width: 50, tree_hide_below: 120,
  info_box_cap: Some(15) }` (serde defaults = today's constants) on the
  four browser variants of BOTH enums; the four panes + `TreeBrowserCore`
  read the args (tree width / hide threshold / info cap; `None` = the
  round-8 uncapped info box). **Backward compat is load-bearing and was
  NOT free**: RON cannot deserialize a struct variant from its unit form,
  so `PaneTypeFile::Deserialize` is manual (serde Content capture +
  `{Variant: ()}` → `{Variant: Seq([])}` rewrite + derived-mirror replay;
  the versioned `__private228` module, pinned by the lockfile) — today's
  bare `Directories`/`Playlists`/`Jellyfin`/`Radio` config syntax parses
  with default args (pinned: `bare_browser_panes_parse_with_default_tree_args`,
  `explicit_tree_args_round_trip`, `default_args_are_today_s_constants`).
  1337/1337 (1328 + 9), warnings 3 baseline, guardrail **60 excl-thin,
  identical pair set** (`tree_args` added to the thin-adapter list).
  Net LOC: src/ui **+145** (57,173 → 57,318), tree **+566** (96,231 →
  96,797; tabs.rs 1277 → 1672 — the serde machinery is the price of the
  compat guarantee). 6.3 construction pattern: **documented decision**
  (`docs/design/Rewrite/new-browser-tab.md`) — new browser tab = config
  block + thin adapter over the shared cores, never a new core; the four
  adapters stay per-backend (radio focus/regions tree, jellyfin shared
  selection/poster, playlists list-shaped pane, directories Downloads —
  a backend enum would fork, §3 rule). **Next: 7 — close-out (done
  2026-08-11, see below).**
- **Phase 7 ✅ (rewrite CLOSE-OUT, 2026-08-11, `4512fbf` + 7.2 + 7.3,
  docs/metrics only, zero code edits):** FINAL LOC comparison
  `24bd883` vs `HEAD` (outline §2.4 + §5.6 per-phase table: src/ui
  56,704 → 57,318 **+614**, tree 95,811 → 96,797 **+986** — LOC-positive
  overall, reported plainly; the user priority is extensibility +
  predictable behavior, LOC is a proxy not a gate); docs/design
  `source_files`/`related` sweep (stale paths fixed, `updated:` bumped —
  queue submodules, marquee/wrap/sub_tab_bar widgets, `select_section.rs`
  gone, README index); HANDOFF → final (this section); notes.md
  rewrite-complete block; `docs/design/Rewrite/REVIEW.md` (new — branch
  state, review recipe, remaining host live-checks, caveats); session log
  entry. 1337/1337, warnings 3 baseline, guardrail 60 excl-thin after
  every commit. **Next → none: the rewrite is complete; `master`
  untouched; the host pushes `rewrite` (user rule: agent never pushes).**

**Remaining host live-checks (rewrite, from plan §8 of phases 4b/5/6 —
see REVIEW.md):** queue tab Audio/Video/Chapters behavior, marks, context
menus, toggle row, scrollbars (4b); controls carousel cycle, lyrics
header cluster + info marquee + wrap, jellyfin overview wrap, property
scrolling line (5); the four browser tabs at ~70 vs wide widths, info
boxes ≈ 15 rows, a config override `tree_min_width: 60` /
`info_box_cap: None` followed by a restart, the round-23 config needing
NO edits (6).

**Known caveat (recorded):** `src/config/tabs.rs` uses
`serde::__private228` (the versioned hidden module the derive uses;
Cargo.lock pins serde 1.0.228) for the manual `PaneTypeFile`
Deserialize. A future `cargo update` past 1.0.228 needs the suffix
bumped — a one-line change (`__private228` → the new version), pinned by
`bare_browser_panes_parse_with_default_tree_args`.

Toolchain env (container): `export PATH="$HOME/.cargo/bin:$PATH"`
`export RUSTUP_HOME="$HOME/.rustup" CARGO_HOME="$HOME/.cargo"`.

## Pending

- **Round 29 (2026-08-12 user request) — IMPLEMENTED host-side,
  VALIDATED 1364/1364, binary installed + live-checked (see
  FEEDBACK-2026-08-12-2.md).** Name the cava PipeWire node via a new
  `node_name` option + LD_PRELOAD shim; s2udio's cava now shows as
  `s2udio-cava` in pw-dump. When merged to main, the shim source
  (`scripts/cava-node-name.c`) + setup.sh build step ship in the clean
  subset.
- **Round 28b (2026-08-12 host live-check fix) — IMPLEMENTED host-side,
  VALIDATED 1361/1361, binary installed (see
  FEEDBACK-2026-08-12-1.md).** ONLY `Shift+Tab` toggles the MPD tab's
  Library/Search mode (new global `ToggleMpdMode` bound to `<S-Tab>` in
  the defaults + the live config.ron + example_config.ron; the MPD pane
  claims it while focused); the round-28 `Tab`/NextTab toggle claim is
  removed, so `Tab`/`E`/`Q` cycle tabs again and the MPD tab is always
  reachable from the keyboard; elsewhere `ToggleMpdMode` is a no-op (the
  queue's `Shift+Tab` chapters toggle untouched).
- **Round 28 (2026-08-12 user feedback) — IMPLEMENTED in the container,
  HOST-VALIDATED (1360/1360) + live-checked 2026-08-12 (see
  FEEDBACK-2026-08-12-0.md).** The Search tab folded into the MPD tab:
  default tabs drop the Search entry and any leftover "Search" config tab
  is hidden from the bar/cycle (`is_tab_hidden`); the MPD tab renders a
  `⭘ Library  ● Search` toggle row at the top (● = active; clickable
  labels; hover-lightens; keyboard = **`Shift+Tab`** per round 28b).
  Startup default = **Library** (not persisted), search-filter state
  **preserved across toggles** (the search pane lives inside the MPD pane
  for the session). Search queries now target the Directories pane; the
  MPD pane forwards `SEARCH` results to its embedded search. Search tab
  removed from the live config.ron; **1360/1360** in-container, warnings
  3 baseline.
- **Phase 2.1 — delta close-out: DONE (2026-08-10, see
  outline §5.1).** (1) Items-box title parity restored: `items_title`
  returns the pre-padded title (directories `" Library"` / `" Downloads"` /
  `" {name}"`, jellyfin `" Items "` / `" {label} "`, radio `" Favourites "` /
  `" Local — closest "` / `" {name} "` / `" {state} "` / `" Stations "`),
  the shared `render_items` format is `"{}({}) "` — titles are exactly
  `" Library(3) "` / `" Items (3) "` / `" Stations (3) "` again. (2)
  Temp-play unification kept + pinned: directories Stop drops the temp
  entry, jellyfin Stop clears `ctx.temp_play_id`, radio `PLAY` sets
  `ctx.temp_play_id` — one regression test each. (3) Docs updated (this
  bullet + the Phase-2 deltas bullet → resolved); full suite green
  **1318/1318** (+6), warnings 3 baseline, similarity guardrail unchanged
  (60 excl-thin pairs, identical pair set); committed as `phase 2.1`.
- **Round 25 (2026-08-10 user live-check) — host-implemented fixes:
  search tab `a`/`d`/`←`/`→` filter↔results navigation, search filter
  hover, video-queue anchor hardening on removal paths (commit pending).
  Validated 1307/1307.**
- **Round 24 (2026-08-10 user feedback) — IMPLEMENTED in the container,
  HOST-VALIDATED + live-checked (2026-08-10, commit `7e1f151`).** (1)
  Esc-deselect everywhere: queue **video** list (`handle_video_action`
  Close arm) and the **MPD** tab (`directories.rs` Close arm) now clear +
  drop anchor (`MarkState::clear_anchor`) + consume, second Esc opens
  settings; +3 queue-video +3 directories tests (consumes / without-
  selection / shift-range re-anchors). (2) Search tab parity
  (`search/mod.rs` + `search/inputs.rs`): mouse multi-select (ctrl+click
  toggle / alt+click range / plain-click clear+re-anchor), marked-row
  rendering + hover via the shared `hovered_item` helper (marked wins),
  dual-pane active-focus convention (results hover highlight in
  BrowseResults, focused filter input in Search); +5 tests. **Host:
  `cargo test --release` (expect 1304), warnings ≤ 3, then commit +
  install + live check.** See
  [FEEDBACK-2026-08-10-17.md](FEEDBACK-2026-08-10-17.md).
- **Round 23 (2026-08-09 user feedback) — configs fully in
  `~/.config/s2udio` + s2udio-owned lyrics library: IMPLEMENTED
  host-side, VALIDATED (1282/1282), binary installed.** (1) All configs
  move to `~/.config/s2udio/` — base `config.ron`, sidecars, `themes/`;
  nothing in `~/.config/rmpc` anymore. One-time migration: legacy base +
  round-19 overlay merge into the full `~/.config/s2udio/config.ron`,
  legacy sidecars/themes copied, legacy base renamed
  `config.ron.migrated-round23`. **Note**: the round-19 overlay had
  never actually parsed (its `#` comments + bare `Option` values break
  ron — the log showed `Failed to read the s2udio config overlay
  ExpectedAttribute` since round 19); the s2udio sections effectively
  ran on embedded defaults. (2) `lyrics_dir` default → `~/.config/s2udio/lyrics`.
  Lookup order: the user's MPD library colocated `.lrc` FIRST (read-only,
  never written), then s2udio's own library, then the index. 171
  s2udio-stamped `.lrc` files were migrated out of `/mnt/20TBHDD/Media/Music`
  into `~/.config/s2udio/lyrics` (user's 1788 hand-made files stayed).
  Live checks pending user restart (lyrics show from the new dir; user
  library files still win).
- **Round 22 (2026-08-09 user feedback) — picker title marker + right
  margin: IMPLEMENTED (isodev), COMMITTED, VALIDATED.** See
  [FEEDBACK-2026-08-09-16.md](FEEDBACK-2026-08-09-16.md): (1) the
  picker title is now **"▶ files — <name>"**
  (`format!("▶ files — {} ", scan.torrent_name)` — "Select" dropped so
  long names truncate cleanly instead of a stray "t"); (2) a
  **1-character margin** between the file list and the scrollbar
  (`torrent_file_picker.rs` render — the list block's right padding is
  1, `options_area`/click targets shrink with it, `scrollbar_area`
  column unchanged). **1280/1280**, warnings 3 baseline, binary md5
  `9932fc1f` installed. Live check pending user restart (no live
  instance).
- **Rounds 20–21 + picker-UX follow-ups — DONE (host-validated,
  user-confirmed).** Round 20 `fabbdeb` + `725633f`; round 21 `52fd4db`
  (Stream/Download/Download-all with download-only jobs, 1275/1275);
  picker Enter→buttons `0153432`; a/d+arrows, clickable buttons,
  double-click activation, scrollbar + wheel `901c17c` (1279/1279,
  warnings 3 baseline, binary md5 `9897cce0` installed, live-confirmed).


- **Round 20 (2026-08-09 feedback) — torrent popup UI polish +
  duplicate-paste fix: IMPLEMENTED, HOST-VALIDATED, COMMITTED
  (`fabbdeb` + follow-up `725633f`).** Wait window trimmed (DL-speed row
  gone; counter + `esc to cancel` stay), picker buttons **Play /
  Download & Play / Cancel** (user note: "Download & Play", not
  "Downloads & Play" — relabeled host-side in `725633f`), duplicate
  paste reuses the scanned engine (canonical full-infohash keys,
  `Arc`-shared engines, landed scans survive the close hook). Host
  `cargo test --release` **1273/1273**, warnings 3 baseline; tracker
  6/6, s2u-mpdris 21/21, mpdris-shim 1/1; s2u-yt green. Binary md5
  `abfca9a6` installed (superseded by round 21, see below). Live
  checks pending user restart.
- **Round 21 (2026-08-09 user feedback, implemented host-side) —
  torrent popup options relabeled + download-only actions.** User
  spec: single-file popup = **Stream / Download / Cancel**; multi-file
  popup = **Stream all / Download all / Select files… / Cancel**
  (the plain single-file action is gone from the multi popup).
  Behavior: **Stream** = play now (old "Play (stream)");
  **Download** / **Download all** = keep the file(s) in
  `~/Downloads/s2udio-downloads` **without playback** — new
  download-only path (`AppEvent::TorrentScannedDownload`,
  `WorkRequest::DownloadTorrent` fresh fallback,
  `WorkDone::TorrentDownloadPrepared`); the download job is now
  multi-file (`TorrentDownload.files: Vec<TorrentDownloadFile>`,
  `download_complete` requires every kept file, `finish_torrent_download`
  moves all of them), and the picker's "Download & Play" now tracks
  **every** marked file (was the first only). Host `cargo test
  --release` **1275/1275** (+2 download tests), warnings 3 baseline.
  Binary md5 `79f722b3` installed. Follow-up (user note): the picker's
  Enter on the file list now moves the cursor to the action buttons
  (Play / Download & Play / Cancel) instead of playing immediately;
  `a`/`d` and ←/→ move between the buttons; buttons are clickable
  (single click focuses, double-click activates); the scrollbar is
  mouse-interactable and the wheel moves the list selection —
  **1279/1279**, binary md5 `9897cce0` installed. Live checks pending user restart
  (popup labels; paste a magnet → Download / Download all land the
  files in s2udio-downloads without opening mpv; Stream all plays the
  season).
- **Round 18 (2026-08-08 feedback) — torrent scan wait: no deadline,
  live counter + speed check, user decides: CLOSED host-side
  (2026-08-09) — container implementation host-validated AND 4
  live-check bugs fixed (see the 2026-08-08 session log round-18 host
  entry): add HTTP timeout + `x-req-timeout-ms` (cold magnets died at
  the 5 s agent timeout), per-engine `--listen-port` +
  `--disable-dht-persistence` (2nd engine "Address in use"), picker
  captures the scan (popup close hook wiped `Ctx.torrent_scans` →
  "Select files never played"), `--disable-persistence` +
  `?overwrite=true` + wait-for-`live` (shared-session restore left the
  torrent `Initializing` → streams 500 → mpv exit 2). **1263/1263**,
  binary `67fadf2d`, user live-confirmed the full multi-file flow.**
  Read
  [FEEDBACK-2026-08-08-13.md](FEEDBACK-2026-08-08-13.md): the metainfo
  wait is **open-ended** (no `max_wait_secs`/5 s deadline — the user
  decides how long a cold magnet may take) and runs on a **dedicated
  scan thread** per item (`WorkRequest::ScanTorrent` gained a per-scan
  `cancel: Receiver<()>`; the work thread spawns it and is never
  blocked; the legacy `PlayTorrent` path got the same open-ended wait on
  its own thread). The popup's Loading row became a live **wait window**
  — `Loading <label>… mm:ss` counter + `DL <speed> · need ≥ <min> KB/s
  ✓/✗` (live `stats/v1.live.download_speed.mbps` → KB/s vs
  `torrent.min_download_speed_kbps`) + `esc to cancel` — refreshed once
  per second by new `WorkDone::TorrentScanProgress` → `Ctx.torrent_scan_progress`
  → `refresh_paste_modal`. Esc/close fires each in-flight scan's cancel
  channel (`Ctx.torrent_scan_cancels`, keyed per item) so the thread
  aborts and drops the engine (rqbit killed, no orphan); multi-item
  pastes stay independent. `max_wait_secs` is now a config relic.
  In-container `cargo test --release` **1258/1258** (+4 net; binary
  warnings unchanged, pre-existing). Container toolchain: rust 1.97.1.
  Host: validate + live-check the wait window on a slow magnet (counter,
  ✓/✗, Esc-cancel, work thread unblocked) + handoff.
- **Round 17 (2026-08-08 user feedback) — torrent UX: loading scan,
  multi-file play, video queue: CLOSED host-side (2026-08-08, commit
  `3ec4584`, validated 1254/1254).** Read
  [FEEDBACK-2026-08-08-12.md](FEEDBACK-2026-08-08-12.md): (1) the
  `[Torrent]` popup section shows **"Loading…"** and scans the torrent
  up front (engine + add + metainfo wait), reusing the scanned engine
  for the play actions (`ScanTorrent` → `TorrentScanned`,
  `Ctx.torrent_scans`, `AppEvent::TorrentScannedPlay`; fresh-spawn
  fallback when the scan is gone); (2) multi-video torrents (season
  packs) get **Play all (N files)** and **Select files…** (new
  multi-select modal `torrent_file_picker.rs`, name + size, indices
  preserved); (3) playing a torrent fills the **Queue tab's Video list**
  with its files (Jellyfin-style: `play_video_entries` with one entry
  per file, per-file stream URLs, torrent name as context via synthetic
  in-memory `ctx.yt_info` entries keyed by the stream URLs — never
  persisted, the URL embeds the rqbit auth token;
  `mpv::session_playlist_shown` shows the session playlist).
  Container toolchain: rust 1.97.1.
- **Downloads folder moved (2026-08-08, user-approved, live-confirmed)**:
  downloads land in `~/Downloads/s2udio-downloads` (outside the MPD
  library); the MPD tab shows the folder as `Downloads` at the top of
  the library from a **disk listing** (re-read on open); Downloads
  files play via **mpv** (MPD cannot play outside `music_directory`);
  MPD queue/playlist stream-replace keeps the stream for out-of-library
  files. Existing 7.2 GB file migrated (verified, source removed).
  `update_dir_wait` removed.
- **Torrent streaming — M1 engine bootstrap: CLOSED host-side
  (commits below, validated 1216/1216, binary `00cd1e7c` installed).**
  Plan: `docs/design/Backend/torrent-streaming.md` (status
  `in-progress`). M1 = `src/core/torrent.rs` (rqbit spawn/kill + auth +
  ureq REST client + `find_free_port` fallback, `Drop` reaps the
  child), config section `torrent:` (`src/config/torrent.rs` + defaults
  + example config), env override `S2UDIO_RQBIT_BIN`, 6 fake-engine
  unit tests. Pull gap: the dev copy left the 2 new `.rs` files
  **untracked** — the diff-based pull could not carry them; copied over
  byte-identical from `~/Projects/s2udio` (read-only). Host fixed:
  `Child` is not `Clone` → `kill(&mut self)`; orphan doc comment in
  `config/torrent.rs`; auth-assertion bug (last-whitespace-token vs
  `Basic <b64>`) that poisoned `RQBIT_ENV_LOCK`; module-level
  `#[allow(dead_code)]` on the M1 bootstrap. Open question 1 resolved
  (config port + scan, no ephemeral port); **question 2 RESOLVED**:
  real-engine smoke against `rqbit 9.0.0-beta.2` installed at
  `~/.local/bin/rqbit` (md5 `aec1c9f3`, static binary, the version the
  engine table pins) exposed 5 API mismatches the container's M1 code
  had — global `--http-api-listen-addr` flag position, object add
  response with numeric `id`, file `length`/positional stream index
  (no `id`/`size`), `live.download_speed {mbps, human_readable}` (new
  `Speed` type), `POST /torrents/{id}/delete` verb (DELETE → 405) —
  all fixed host-side; fake fixtures now serve the real v9 shapes;
  `docs/design/Backend/torrent-streaming.md` facts corrected; still
  1216/1216. Session log: `docs/design/Sessions/2026-08-08.md`.
- **Torrent streaming — M2 classification + popup + "Play and Download":
  CLOSED host-side (2026-08-08; validated **1238/1238**, binary
  installed).** `PastedItem::Torrent`
  (local/`file://`/`http(s)` `.torrent`) + `PastedItem::Magnet`
  (infohash labels + dedupe), `[Torrent]` popup section with **Play
  (stream)** and **Play and Download** (dim `Torrent streaming
  disabled` row when `torrent.enabled: false`), end-to-end action on
  the work thread (engine start → add → `wait_for_files` ≤ 5 s for
  magnet metainfo → `pick_playable_file` → stream URL) with the engine
  kept in `Ctx.torrent_engine` and playback via mpv. **Play and
  Download** (host addition 2026-08-08): a `Ctx.torrent_download` job
  polls `stats/v1` (1 s); on completion the picked file moves to
  `~/Downloads/s2udio-downloads` (deferred to `MpvSessionEnded` when
  the stream is still playing), the torrent is deleted from the
  engine. Now-playing/MPRIS: the file name is the title (instead of the
  raw stream URL) and the torrent name is the artist; no art yet (M4).
  Also fixed: the runtime default `torrent.cache_dir` is now
  tilde-expanded (without a `torrent:` config section the engine used a
  literal `~` path). **`stream_url` embeds the auth token as URL
  userinfo** — verified live that rqbit 9.0.0-beta.2 enforces auth on
  the stream endpoint (401 without, 206 with). Host re-ran `cargo test
  --release` **1238/1238** (M2's 16 + 6 new: download-completion,
  file-move, v9-stats parse, config expansion, updated popup tests; 3
  warnings, all pre-existing at HEAD) + suites 6/6 + 21/21 + 1/1 +
  real-engine smoke against the pinned `rqbit 9.0.0-beta.2` (spawn,
  numeric-id add, `{name,length}` files, largest-video pick, userinfo
  stream 206 / no-creds 401, POST delete 200 / DELETE 405 — all
  facts hold). M3 (bandwidth gate) inserts the speed check before
  playback; M4 (picker, cache args, yt_info, lifecycle) refines.
  Docs + session log updated. **Next: M3 (bandwidth gate) — stats
  polling + progress modal + dialogs.**
- **Round 14 (2026-08-08 user report) — Jellyfin episode metadata
  mismatch: CLOSED host-side (commit `7570d37`, validated 1210/1210,
  fix + tab-nav batch committed; working tree pulled clean). Full
  record in
  [FEEDBACK-2026-08-07-11.md](FEEDBACK-2026-08-07-11.md): while
  playing Jellyfin TV episodes ("Play with MPV" from the Jellyfin
  tab) the TUI and MPRIS show the **next** episode's title while mpv
  plays the selected one — DS9 S04E02: mpv says "The Visitor"
  (correct), TUI/MPRIS say "Hippocratic Oath" (S04E03); S04E03: mpv
  says "Hippocratic Oath", TUI/MPRIS say "Indiscretion" (S04E04).
  Exactly +1 in both reported cases, "sometimes" per the user; mpv's
  own title is correct → the bug is s2udio-side current-item /
  playlist-position handling in the season-playlist flow (and
  whatever MPRIS mirrors). Data-flow pointers in the feedback file.
  **Root cause (proven)**: mpv's `loadfile … replace` swaps the
  **current entry only**, keeping the rest of the old playlist — when
  a same-length old season is already loaded at position k>0 (DS9
  seasons are all 26), the clicked file splices into it at k, but
  `JF_SEASON_PLAY` records the rotated new season (clicked first, pos
  0). The poll's count gate passes (26 == 26) and indexes the rotated
  recorded playlist by mpv's stale position k → the **next** episode
  (+1). Fix: rebuild mpv's playlist on every switch (`mpv_playlist_clear`
  - reload, in `JF_SEASON_PLAY` active branch, `play_video_entries`,
  pending-loadfile), and harden the poll advance with
  `recorded_entry_for_mpv_pos` (Jellyfin item ids vs mpv's live
  `path`; confirmed mismatch skips the advance). Tests: entry
  confirmation (match / stale refusal / genuine advance / YouTube
  fallback), `read_mpv_path`, `mpv_playlist_clear`. Full record in
  the session log; `docs/design/Backend/mpv-session.md` updated.
- **Round 12 (2026-08-07 live-check feedback) — lyrics buttons
  live-check round 2: CLOSED (implemented in the container, pulled,
  host-validated, live-check fixes applied host-side — hover excludes
  the leading space, held-⭘ survives the fetch completion — plus the
  round-13 layout round, all closed at 1208/1208 with binary
  `7cf770e2` installed and live-checked).** Three items from
  the round-11 live check (user confirmed "button positions and
  styling look good"): (1) **hover highlights the label text only** —
  the glyph (`●`/`⭘`) keeps its completely normal style (no hover
  background, no bold, no brightening); only the label text
  gets the hover treatment (bg + bold + brightening); (2) **`⭘` must
  persist while the button is held** — real bug: the 300 ms
  release-check one-shot fires mid-hold on kitty (kitty sends
  `Up(Left)` on release) and reverts the marker while the button is
  still down; gate the fallback on `crate::shared::terminal::
  TERMINAL.emulator()` (kitty-family → rely on `LeftRelease`, no
  one-shot; `Unknown` → keep the fallback); the action still fires
  once per press, never repeats; (3) **rename + margin + hidden info
  panel**: `wrong lyrics` → `hide lyrics` (same behavior), a
  one-character right margin (`start = width − cluster_w − 1`), and
  when the lyrics are hidden the pane shows the **paused-style info
  panel** with the button switched to `● show lyrics` (mockups in
  `FEEDBACK-2026-08-07-10.md`). Behavior stays as validated
  (wrong-mark hides, fetch clears + refetches, per-song in-session,
  mouse only, fire once per press). Update the round-11 tests; keep
  1205/1205.
- **Round 11 (2026-08-07 live-check feedback) — lyrics buttons visual/UX
  refinements: CLOSED (implemented `c83dcb4`, host-validated
  1205/1205, binary `f9d3532e` installed, live-checked; findings →
  round 12).** Five changes from the
  round-10 live check: (1) remove the `Artist - Title` header (not
  requested — buttons only on the top row, lyrics body below), (2) `⭘`
  is pressed-while-held only (reverts to `●` on release; no repeat
  execution while held; no persistent ⭘ for wrong-mark or fetch
  in-flight), (3) cluster styled `● wrong lyrics | ● fetch lyrics`
  (space-padded `|`), (4) collapse to `● wrong | ● fetch` when narrow
  (hide only if even that doesn't fit), (5) hover = the queue-list row
  highlight (bg + bold via `hovered_item_style`) on the whole button
  **plus** label-text brightening — the `●`/`⭘` glyph keeps its normal
  foreground (only bg/bold from the hover), the `|` separator is never
  highlighted. Behavior stays as validated
  (wrong-mark hides, fetch clears + refetches, per-song in-session,
  mouse only). Full record `FEEDBACK-2026-08-07-9.md`; reworked the 6
  round-10 tests (cluster, collapse, pressed-while-held, release-check
  fallback, hover); keep 1202/1202. New `MouseEventKind::LeftRelease`
  (mapped from the terminal's left-button Up, previously dropped) +
  `UiAppEvent::LyricsReleaseCheck` fallback one-shot for terminals
  without release events.
- **Round 10 (2026-08-07 feedback) — lyrics panel buttons: `wrong
  lyrics` + `fetch lyrics` — CLOSED (implemented `96bf7be`, host-
  validated 1202/1202, binary installed, live-checked; findings →
  round 11).** Two mouse-only buttons on the lyrics pane's top row
  (`src/ui/panes/lyrics.rs`), each with a ●/⭘ pressed marker and the
  standard hover: `wrong lyrics` marks the current song's lyrics wrong
  and hides the body (per-song, in-session; a second click clears the
  mark); `fetch lyrics` refetches via the configured `on_song_change`
  command (`rmpc-fetch-lyrics` default), clears the wrong-mark
  immediately (hidden lyrics reappear) and shows ⭘ until the lyrics
  index reloads. Mouse only, no keybinds; +6 tests; host ran `cargo
  test --release` (1202/1202 after 5 test borrow-checker fixes).
  Full record `FEEDBACK-2026-08-07-8.md`; layout spec updated in
  `docs/design/Frontend/layout-templates.md` (to be revised for round
  11).
- **s2u-yt quality round (2026-08-07): CLOSED, live-verified** — streaming
  - seeking confirmed working by the user (see Current state). No app code
  involved; the running s2udio binary `8cd0da1e` is current.
- **Round 9 fully closed (2026-08-07)**: the info-box cap was confirmed
  visually by the user (MPD/Playlists info boxes ~15 lines, lists fill the
  rest) — all rounds 7–9 live checks now complete.

- **Round 7 (2026-08-07 feedback) — two MPD-tab UI items: implemented,
  host-validated (1190/1190, 4 test fixes), binary installed
  `cb545ec6`; live UI checks pending user restart.** `Enter` opens the
  right-click context menu on folders AND songs (parity with
  Playlists/queue; `d`/`→` stay open/play); left folder-tree pane min
  width **50 chars** via `tree_width()`, **hidden entirely when the
  TUI is ≤ 120 cols** (right pane keeps ≥ 1 col; tree never rendered →
  inert mouse). Live check after restart: kitty ~70 cols hides the
  tree, a wide terminal shows a 50-col tree, Enter opens the menu on a
  folder and a song, `d`/`→` still open/play.
- **Round 8 (2026-08-07 feedback) — left-pane min-width/hide →
  Playlists + Jellyfin tabs + info-box 2/3 user note:
  host-validated and closed** (1194/1194, 6 host test fixes, binary
  installed `77a5ef0`). Shared `tree_width()` helper (was MPD-only in
  `directories.rs`, now `pub(crate)`) used by all three browser panes;
  `playlists.rs` + `jellyfin.rs` render with the same min-50 /
  hidden-≤-120 split instead of the fixed 30/70; when the left pane is
  hidden its rect stays `Rect::default()` (jellyfin resets `tree_area`
  explicitly), so **scroll over the left columns drives the right
  pane**; the **MPD and Playlists info boxes take ≈ 2/3 of the pane
  height** (exact-length split; tips strip stays 3 rows; other tabs'
  info boxes unchanged). New tests: `left_pane_hidden_on_narrow_tui` /
  `left_pane_keeps_min_width_on_wide_tui` in `playlists/tests.rs`;
  `tree_pane_hidden_on_narrow_tui` / `tree_pane_keeps_min_width_on_wide_tui`
  in `jellyfin.rs`. **Live checks pending user restart**: kitty ~70
  cols hides the left pane in Playlists/Jellyfin (scroll over the left
  columns drives the right pane), a wide terminal shows a 50-col pane,
  MPD/Playlists info boxes ≈ 2/3 height. Full record
  `FEEDBACK-2026-08-07-6.md`.
- **Round 9 (2026-08-07 feedback) — MPD/Playlists info box max height
  15 lines: closed host-side** (1196/1196, 0 fixes needed; the
  container's uncommitted implementation was committed as-is with
  only doc closure updates). Round 8's info box was `(h−3)×2/3`,
  unbounded (at h=60 that's 38 rows); capped at 15 with `.min(15)`
  on `info_h` in `directories.rs` + `playlists.rs` (the files/songs
  list takes the remainder). New tests: `info_box_height_cap` module
  in `playlists/tests.rs` — tall render 160×40 →
  `info_area.height == 15` (songs list gets 20 inner rows), short
  render 160×20 → uncapped `info_h == 11` (round-8 behavior
  unchanged). Jellyfin and other tabs unchanged. Binary installed
  `8cd0da1e`; **live check DONE: user confirmed the MPD/Playlists info
  boxes are ~15 lines tall at the kitty height, lists fill the rest.**
  Full record `FEEDBACK-2026-08-07-7.md`.
- **Round-3 (2026-08-07 feedback) validated by host**: cava deferred
  start, `ScrollbarDrag` (click-jump + 1:1 thumb drag) on every themed
  scrollbar, `modal_mouse_pos` hover split — full suite 1185/1185,
  user confirmed scrollbar behavior as intended. Container's round-3
  tree was subsumed by the host commit; stash dropped.
- **Round-4 (2026-08-07 feedback) — build+tests host-validated, live
  checks pending user restart**: settings keyboard-nav parity with tab
  panes (`SettingsFocus` Sidebar/Content; w/s/↑/↓ move the focused
  pane, d/→/Enter opens / toggles, a/← back to sidebar — note: ←/→
  value-adjust dropped, steppers adjust via d/→/Enter in content focus,
  decrease is mouse-only); seekbar hover highlights only the bar left of
  the cursor; queue Ctrl+Tab → seekbar focus (`FocusSeekbar` action,
  cursor at playback pos, a/d+arrows move ±2s, tap Space/Enter seeks +
  returns focus, hold Space = interactive seek: arrows only, audio ±2s
  L/R ±5s U/D @1Hz auto-repeat, video ±5s U/D + frame-by-frame L/R; new
  `REPORT_EVENT_TYPES` + release-check fallback for non-kitty
  terminals); queue-list hover applies to right-click/Enter context menu
  (`hovered_item_style`). Host fixed 22 compile/test errors (no
  toolchain in container) + aligned 3 never-run test assertions to the
  spec-correct rendering; `cargo test --release` **1187/1187**, suites
  6/6 + 1/1 + 21/21; binary installed md5 `10cd86d1`. **Live checks
  pending (user restart)**: settings nav + dimmed sidebar, seekbar
  hover, Ctrl+Tab tap/hold/frame-step on kitty + a non-kitty terminal,
  plain Tab still switches tabs, context-menu hover. Round-3 nits also
  fixed: `calculate_scrollbar_position` removed (dead), the queue.rs
  unused `mut` and ignored `Result`s, and the browser.rs no-op `drop`.
- **MPRIS validation plan** (`docs/design/Validation/mpris-validation.md`):
  standing gate for the next MPRIS round — no MPRIS code changed this
  round; the plan's automated gates (tracker/shim/mpdris suites) all
  pass.
- **MPRIS first live run (2026-08-07) — scenario A FAIL + stale
  installs**: full record `FEEDBACK-2026-08-07-3.md` + session log.
  (1) **Baseline FAILED**: installed scripts ≠ repo (installed
  `s2u-mpdris2` is the round-1 shim, 4,325 B — missing round-2 seekid/
  yt-info/art-retry). **Host must reinstall the 3 repo scripts and
  re-verify the §4 md5 pairs before the next live run.** (2) **Scenario
  A FAIL (user-reported)**: local MPD library file shows title +
  controls but **no art** (`mpris:artUrl` absent). Code trace: shim
  passes local files to official `find_cover()`; likely causes in order:
  mpDris2 `music_dir` misconfigured (URL never `file://`), mutagen
  missing (embedded art skipped), or file genuinely has no embedded /
  adjacent cover (`^(album|cover|\.?folder|front).*` — `art.jpg` does
  NOT match). 3 host-side checks in the feedback doc; root cause
  unproven.
- **MPRIS scenario A — ROOT CAUSE PROVEN (2026-08-07)**: shim bug in
  `scripts/s2u-mpdris2` `_patch_mpdris2()` — `find_cover` closure reads
  the cell `_orig`, which the Seek/SetPosition loop later rebinds to
  `MPRISInterface.SetPosition` → `TypeError` (swallowed) → **no
  `mpris:artUrl` for every local `file://` track**; streams unaffected
  (art set directly by retry). One-line fix (`def find_cover(self,
  song_url, _orig=_orig):`), **hot-verified live** on the host (artUrl
  per track, valid JPEGs; backup `/tmp/s2u-mpdris2.bak`). Full record
  `FEEDBACK-2026-08-07-4.md`. Host baseline was already green (the
  container's stale-install verdict was container-local); music_dir
  config was NOT the cause. **CLOSED (2026-08-07)**: the one-line fix
  was committed host-side (container had applied it but not pushed);
  repo `scripts/s2u-mpdris2` == installed `bc2bb231` (byte-identical,
  §4 pair restored), py_compile + shim 1/1, `mpDris2.service`
  restarted, scenario A + stream→local transition re-verified live.
- **Setup is official-packages-only**: no patched cava / patched mpDris2 /
  yt-dlp-ejs anymore — setup.sh installs repo `cava` + `python-yt-dlp` and
  AUR `mpv-full` + `mpdris2-git`. mpv MPRIS is the bundled `s2udio-mpris`
  daemon started by the tracker; stream art in the MPRIS slot is the
  bundled `s2u-mpdris2` shim (runs the official binary, extends
  `find_cover()` at runtime; falls back unpatched on upstream refactors).
  Remaining known gap: some YouTube format resolutions may fail without
  yt-dlp-ejs.

## Operational gotchas (kept here — not in the spec docs)

- **s2u-yt wrapper phases**: a plain `yt-dlp` call goes through the two-phase
  wrapper (anonymous android_vr → authenticated web_safari on failure).
  `--ignore-config` is used, so the effective config is exactly what the
  wrapper passes. If android_vr URLs start 403ing or `status.sh` reports an
  HLS fallback, check bgutil first: `systemctl --user status
  s2u-yt-bgutil.service` / `./status.sh`.
- **Clipboard paste (Ctrl+V / middle click)**: the app reads the system
  clipboard itself (mouse capture swallows the terminal's own paste) via
  `wl-paste` with `xclip`/`xsel` fallbacks — **this box only has
  `wl-paste`**, so an unreadable/empty primary selection is a silent
  no-op. Non-media content is ignored by design (no popup). If paste
  "stops working", check the primary selection first, then a restart.
- **Never assume a fixed queue**: before any test that plays something,
  snapshot + restore — `mpc save .s2u-backup` before; `mpc clear &&
  mpc load .s2u-backup && mpc rm .s2u-backup && mpc stop` after.
- MPD `playlistadd`/`delete`/`save` DROP `#EXTINF` names → favourites
  (`radio.m3u`) mutations rewrite the `.m3u` directly (MPD hot-reloads
  via inotify). The `config` MPD command is TCP-restricted → playlist
  path falls back to `~/.config/mpd/playlists`. MPD refuses absolute
  local paths over TCP → pasted/dropped files are relativized against
  `music_directory`. `addtagid` **appends** → `add_tag_id` sends
  `cleartagid` first.
- **MPRIS** = official **mpdris2-git** (`/usr/bin/mpDris2`, unit
  `mpDris2.service`, AUR / CachyOS repos) run through the bundled
  **s2u-mpdris2** shim (`~/.local/bin/s2u-mpdris2`, drop-in
  `mpDris2.service.d/s2udio.conf` swaps ExecStart; version-guarded, falls
  back unpatched) + the bundled **s2udio-mpris** daemon
  (`~/.local/bin/s2udio-mpris`, D-Bus `org.mpris.MediaPlayer2.s2udio`,
  spawned by the mpv tracker and respawned every 30 s while a video is
  alive) for mpv video. **The tracker runs `mpDris2.service` only while
  MPD is the active MPRIS source (mutual-exclusion winner, followed
  every tick in both s2udio-running and caretaker modes): video playing
  (or paused with no MPD playback) → mpDris2 stopped (no stale
  paused-audio entry); MPD playing with the video paused (audio wins) →
  mpDris2 started; mpv exit → mpDris2 started.** The bridge's
  `Properties.Get`
  serves **every** MPRIS Player property incl. `Position` + `Volume`
  (missing keys reply None → dbus TypeError → clients see no info at
  all — that exact bug made the video entry invisible in the panel and
  was fixed 2026-08-07); `Set(Volume)` routes to mpv (0..1 → 0..100).
  The bridge's `mpris:artUrl` is **mtime cache-busted** (`?t=<mtime_ns>`)
  — the poster is rewritten in place per entry, and KDE caches art by
  URL, so without the query the previous video's thumbnail would linger
  (YouTube → Jellyfin).
  Audio: s2udio re-tags streams
  (`cleartagid`/`addtagid`) so MPD's current song carries the real
  title/artist; the `mpris-art` write is expected-source-guarded and the
  shim serves it cache-busted (`file://…?t=<mtime_ns>`, so KDE re-fetches
  on stream change) for non-file URLs. Video: `mpv-mpris.json`
  (poll-written) + `mpris-mpv-art`; the bridge exits when the state file
  is stale (> 10 s) or gone, so no stale video MPRIS lingers. Local-file
  covers come from mpDris2 itself.
- mpv: **SVP support toggle** (Settings -> mpv -> "svp support";
  config.ron `mpv.svp`, default off, persisted to state.ron). On: s2udio
  launches mpv with `--input-ipc-server=/tmp/mpvsocket` and tracks
  playback over that **fixed socket** — the same socket SVP4's manager
  connects to for frame interpolation (one mpv, one socket, both
  clients). Off (default): no flag, mpv's IPC socket comes from the
  user's own mpv.conf / scripts. mpvSockets.lua is no longer installed:
  its runtime override of `input-ipc-server` killed `/tmp/mpvsocket` out
  from under SVP. Discovery: live `/tmp/mpvsocket` first, then legacy
  `/tmp/mpvSockets/<pid>`, then the stale fixed path. Reattach zombie:
  `mpv_exchange` returns `None`
- **mpv binary**: config.ron `mpv.bin` selects the player binary
  (default `"mpv"`). With SVP4 the host uses SVP's bundled mpv
  (`~/.local/bin/SVP4/mpv/mpv`): it carries SVP's own portable
  VapourSynth (core R73 / API R4.1) + Python 3.12, which the SVPflow
  plugins are built against — the distro VapourSynth 77 + Python 3.14
  stack made mpv SIGSEGV in `libsvpflow2`/`libvstrt` ~20-30 s into
  playback. SVP4's `deps.python` (3.12.11), `deps.vapoursynth` (v72)
  and `opt.mpv` (0.41.0) components are installed into the SVP folder
  via `svp4-maintenance --installPackages ...` (headless, with
  `QT_QPA_PLATFORM=minimal`).
  on a failed connect; the poll synthesizes `MpvSessionEnded` after 5
  stale reads. Temp-play cleanup fires on `PlaybackStateChanged` (after
  `ctx.status` refreshes), not `Player`.
- **mpv SIGABRT `vo_x11_init: Assertion !vo->x11` (crash, core dump,
  socket left behind)** = NVIDIA driver/library mismatch: the loaded
  kernel module (check `/proc/driver/nvidia/version`) is older than the
  installed `nvidia-utils` (check `nvidia-smi` → "version mismatch") —
  happens when a driver upgrade lands without a reboot. Vulkan init fails
  (`VK_ERROR_INITIALIZATION_FAILED`), EGL falls back to llvmpipe, and
  mpv 0.41's gpu-next re-inits the X11 VO on "suspected software
  renderer" (upstream issue #17190). Fix: reboot. Verify with
  `mpv --no-terminal <file>` (it must play, not abort).
- **`replace_id` fetches are fire-and-forget**: a queued query is skipped
  when a newer one shares `(id, replace_id, target)` — the skipped
  callback never runs. Pane-level `pending` bookkeeping must clear itself
  when a result arrives.
- Menu modals start with **no selection** (first Down selects item 0).
- `keybinds.ron` is written by Settings; the config watcher ignores it,
  but its remove/override maps re-merge on every config reload.
- Overlays (art, poster, cava, MPRIS poster): hidden under modals /
  resizes; displayed only after the frame's buffer flush; art re-places
  only when that frame's diff rewrote its cells.
- pkill/pgrep patterns with literal paths self-match the calling shell —
  use `[c]ava`-style bracket tricks.

## Documentation maintenance (methodology)

Role split: **docs/design/ = the spec** (durable behavior),
**HANDOFF.md = the operating state** (session facts, gotchas, pending).
Both stay lean; drift is the enemy.

### Routing table — where a fact goes

| Fact type | Home |
| --- | --- |
| Durable behavior (how a pane / source / flow works) | the matching `docs/design/` doc; bump its `updated:` and fix `status:` |
| Verified session state (test count, build hash, live config values, git state) | HANDOFF → Current state |
| Non-obvious operational pitfall learned the hard way | HANDOFF → Operational gotchas |
| Open work / next steps with blockers | HANDOFF → Pending (revisit at session start) |
| User design decisions | HANDOFF → User design decisions **and** reflected in the relevant docs |
| Session narrative (what changed, decisions, pitfalls, closing state — in order) | `docs/design/Sessions/YYYY-MM-DD.md` |

### Session log (create & maintain)

- **Start**: create today's file if missing; repeat sessions on one day
  append a `## Session <time>` block.
- **Content**: what changed (files), key decisions and why, pitfalls,
  closing state (test count, git status, uncommitted/installed work).
  Bounded — a few lines per topic; never restate a spec doc, link it.
- **End**: verify the entry is written; bump the file's `updated:` when
  you edit it later.

### Rules

- **One home per fact.** The handoff never restates spec — it points to
  it.
- **Handoff diet**: target ≤ ~200 lines / ~15 KB; migrate overflow into
  docs and leave a pointer. Refresh numbers every session; delete
  superseded facts.
- **Same-session doc sync**: when code changes behavior, update the doc
  in the same session — never leave "docs say X, code does Y".
- **Doc lifecycle**: bump `updated:`, fix `status:`, keep `source_files`
  - `related` truthful; new subsystems get a doc + rows in
  `docs/design/README.md` + this map.
- **End-of-session checklist**: route changes through the table above,
  then verify — test count, `git status`, spec-map paths exist, today's
  session log written.
- **Conflict rule**: docs win for behavior, handoff wins for state; fix
  the loser immediately.

## Housekeeping

- Source layout: `src/radio/`, `src/jellyfin/`,
  `src/config/{radio,jellyfin,video,mpv}.rs`,
  `src/ui/panes/{radio,jellyfin,queue,queue_header}.rs`,
  `src/ui/modals/{paste,settings,remap_keys,tab_help}.rs`,
  `src/core/{blur,mpv,command,event_loop}.rs`.
- Keep the README fork section and the docs index in sync when the spec
  changes (see the methodology above).
