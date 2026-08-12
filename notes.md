# Notes for the container agent — round 28 IMPLEMENTED (isodev, 2026-08-12)

## ROUND 28 — IMPLEMENTED in the container (2026-08-12) — awaiting host validation

**User feedback (2026-08-12) →
[FEEDBACK-2026-08-12-0.md](FEEDBACK-2026-08-12-0.md).** Fold the Search
tab into the MPD tab: removed the top-level Search tab (default tabs;
leftover "Search" config tabs are hidden via `is_tab_hidden`) and added
a `⭘ Library  ● Search` toggle inside the MPD tab switching between the
unchanged MPD library view and the folded-in Search UI (rounds 24–27
search behaviors carried over; still queries the MPD library). Toggle =
mouse clicks on the labels or **`Tab`** while the MPD tab is focused
(`E`/Shift+Tab still cycle tabs); startup mode = Library; search state
(filters/results) survives Library↔Search toggles for the session.
Search queries retargeted to the Directories pane, which forwards
results to its embedded search. **1360/1360**, warnings 3 baseline.
Host: validate + remove the Search tab from the live config.ron +
install + live-check.

---
# Notes for the container agent — REWRITE MERGED to main (2026-08-11)

## REWRITE MERGED to main (2026-08-11) — host push

The UI-reuse rewrite (phases 0-7) is now **the production `master`**
(`NJMRgit/s2udio` @ `bcf742c`, clean buildable subset — a merge commit
carrying both the old release `a5cd24d` and the rewrite lineage, full
history preserved). The full tree lives on this private repo's `working`
branch @ `05a3e94` (rewrite phases 0-7 on top of the round-23..27
lineage); the separate `rewrite` branch was folded into `working` and
deleted on 2026-08-11. Prior master release preserved at
tag `master-20260811-pre-rewrite` (`a5cd24d`).

Host final assessment (2026-08-11): gate reproduced exactly — **1337/1337
tests, 3 warnings (release build), similarity guardrail 60 excl-thin,
src/ui 56,704 → 57,318 / tree 95,811 → 96,797** vs `24bd883`; all pending
live-checks (phases 4b/5/6) passed: queue sub-tabs/context menu/marks,
controls carousel marquee (scroll + 5-col-gap wrap), lyrics pane +
`● hide lyrics | ● fetch lyrics` cluster, jellyfin video info marquee +
`Description ↴` wrap, the four browser tabs at 70/150 cols, and the
`tree_min_width: 60` / `info_box_cap: None` config override (restored
after). Round-23 config parsed with no edits.

**Next: nothing on `rewrite`; no new rewrite work. Master is the rewrite.**
Known caveat carried over: `serde::__private228` in `src/config/tabs.rs`
needs a one-line suffix bump on any `cargo update` past serde 1.0.228.

---
# Notes for the container agent — round 23 CLOSED (host)

## REPO LAYOUT (2026-08-09 restructure — READ BEFORE PUSHING)

The dev tree now lives in its OWN PRIVATE repo — there is no `working`
branch on the public/distribution repo anymore.

- **`NJMRgit/s2udio`** — distribution repo, **`master` branch only** =
  the clean buildable subset: ONLY what a user needs to build and run
  the TUI. Omit on every push to master: `docs/`, all agent `.md`
  files (`FEEDBACK-*.md`, `HANDOFF.md`, `notes.md`), `.github/`,
  `.vscode/`, `.typos.toml`, `tests/`, anything not required to
  build+run. Keep: `Cargo.toml`/`Cargo.lock`/`build.rs`/
  `rustfmt.toml`, `LICENSE`, `README.md`, `.gitignore`, `src/`,
  `assets/`, `scripts/`, `setup.sh`, `s2u-mpdris/`, `s2u-yt/`.
- **`NJMRgit/s2udio-working`** — PRIVATE dev repo, branch `working` =
  the FULL tree (this repo): docs/, HANDOFF/notes/FEEDBACK, .github/,
  host test suites, editor/lint configs — everything. Push the complete
  tree here on every round.
- `assets/example_config.ron` and `assets/default.jpg` are compiled into
  the binary (`include_str!`/`include_bytes!`) — never delete them.
- The pre-restructure history is preserved locally at the
  `pre-restructure` tag.

---
Date: 2026-08-11 · Branch: `rewrite`

## UI REUSE REWRITE — COMPLETE (Phases 0–7, close-out 2026-08-11)

The UI-reuse rewrite (`docs/design/Rewrite/ui-reuse-rewrite.md`) is
**done** on branch `rewrite` (private `NJMRgit/s2udio-working` only;
distribution `master` untouched). Working tree is clean at HEAD; the
branch is **ahead 38 of `origin/rewrite` (which sits at the Phase-0
baseline `24bd883`) — UNPUSHED by design (user rule: the agent never
pushes; the host pushes separately).**

- **Phases 0–7 all ✅** — one implementation per shape via the shared
  cores: `SongListCore` (Phase 1), `TreeBrowserCore` (Phase 2), `ListModal`/
  `InfoListModal` masters (Phase 3), `SubTabBar` (4a), queue.rs split into
  `queue/{context_menus,video,chapters}.rs` (4b), marquee/wrap widgets
  (Phase 5), `TreeBrowserArgs` config args (Phase 6), close-out docs +
  metrics (Phase 7). Full commit list + numbers: REVIEW.md.
- **Gate at HEAD: 1337/1337 tests** (baseline 1312), warnings 3 baseline,
  similarity guardrail **60 excl-thin pairs**. No code changed in Phase 7.
- **LOC is LOC-positive overall, reported plainly**: src/ui 56,704 →
  57,318 (+614), tree 95,811 → 96,797 (+986) vs the `24bd883` baseline —
  the master-module pattern front-loads the shared cores; the user
  priority is extensibility + predictable behavior (LOC is a proxy, not
  a gate). Per-phase ref-to-ref table: outline §5.6.
- **Host live-checks still pending** (phases 4b/5/6 — queue tab
  behaviors; controls/lyrics/jellyfin marquee+wrap; the four browser
  tabs' tree args at ~70 vs wide widths + a config override + the
  round-23 config parsing with NO edits). List: REVIEW.md.
- **Serde caveat**: `src/config/tabs.rs` uses `serde::__private228`
  (lockfile pins serde 1.0.228); a future `cargo update` past 1.0.228
  needs the suffix bumped (one line).
- **Next: nothing on `rewrite`.** The host reviews (REVIEW.md), runs the
  live-checks, then pushes. Do NOT start new rewrite work; do NOT push.

---
Date: 2026-08-10 · Branch: `working`

## RELEASED to main (2026-08-10) — rounds 23-27 on NJMRgit/s2udio master (`a5cd24d`)

## ROUND 24 — IMPLEMENTED in the container (2026-08-10), HOST-VALIDATED (1304/1304) + live-checked

**User feedback (2026-08-10) →
[FEEDBACK-2026-08-10-17.md](FEEDBACK-2026-08-10-17.md).** Both items
implemented in the container (working tree @ `efeb87e`, uncommitted):

1. **Esc-deselect everywhere**: queue **video** list
   (`handle_video_action`) and the **MPD** tab (`directories.rs
   handle_action`) got the audio-queue/browser Close arm —
   `Close if !marks.is_empty()` → clear + `MarkState::clear_anchor()`
   (new method, drops anchor + last range) + `event.consume()` + render;
   second Esc opens settings. +3 queue-video tests, +3 directories tests
   (esc consumes / esc-without-selection / shift-range re-anchors after
   Esc).
2. **Search tab parity** (`search/mod.rs` + `search/inputs.rs`):
   ctrl+click toggle / alt+click range / plain-click clear+re-anchor on
   the results list (both phases), marked-row rendering (marked style via
   `to_list_items`, hover via the shared `hovered_item` helper, hover
   never overrides marked), hover-on-selected highlight switch, and the
   dual-pane focus convention — results selection uses the hover
   highlight in BrowseResults, the focused filter input in the Search
   phase (`InputGroups` gained `focused_style` + `pane_focused` render
   param). +5 tests (ctrl/alt/plain click, marked render, hover render,
   focused-pane).

**Not validated** — no Rust toolchain in the container (tree-sitter
parse clean on all 5 modified files). Host: `cargo test --release`
(expected **1304** = 1293 + 11 new; warnings ≤ 3 baseline), then commit,
install, live check.

---
Date: 2026-08-09 · Branch: `working`

## ROUND 23 — DONE HOST-SIDE, do not re-implement

**User feedback (2026-08-09, direct to the host):** (1) all configs move
to `~/.config/s2udio/` — no more `~/.config/rmpc`; (2) the lyrics
directory becomes `~/.config/s2udio/lyrics` (s2udio's own .lrc library;
check the user's MPD library FIRST, read-only, never overwrite user .lrc
files). Implemented host-side: `paths.rs` (config_dir → s2udio),
`config_read.rs` (one-time migration: legacy base + round-19 overlay
merge → full `~/.config/s2udio/config.ron`, legacy sidecars/themes
copied, legacy base renamed `config.ron.migrated-round23`; broken
overlays are warned-and-ignored like before), sidecar legacy fallbacks
repointed, `lyrics_dir` default + torrent cache default moved, lyrics
lookup order in `ctx.rs`, lyrics-dir auto-create, `s2u-mpv-tracker`
reads `~/.config/s2udio/config.ron` first, `setup.sh` seeds
`~/.config/s2udio/`. **1282/1282** (+2 tests), warnings 3 baseline.
171 s2udio-stamped .lrc migrated out of the library. Upstream-rmpc
leftovers removed (CHANGELOG/CONTRIBUTING/FAQ + placeholder art +
rmpc.desktop; `assets/default.jpg` and the example configs stay — they
are compiled in). Live checks pending user restart. **Important history**: the round-19 overlay never
parsed (`#` comments / bare `Option` values → `ExpectedAttribute`
warning since round 19) — the s2udio sections effectively ran on
embedded defaults; the migration writes one correct full config.
**Nothing for you to implement.**

---
## Previous — round 22 CLOSED (isodev)

Date: 2026-08-09 · Branch: `working`

## ROUND 22 — IMPLEMENTED (isodev), do not re-implement

**User feedback (2026-08-09) → [FEEDBACK-2026-08-09-16.md](FEEDBACK-2026-08-09-16.md).**
The two picker nits are now fixed host-side and committed on `working`:
(1) the picker title is **"▶ files — <name>"** (`format!("▶ files — {}
", scan.torrent_name)` — no more stray "t" on long names); (2) a
**1-character margin** between the file list and the scrollbar (list
block's right padding = 1 in `torrent_file_picker.rs` render;
`options_area`/click targets shrink with it, `scrollbar_area` stays).
`cargo test --release` **1280/1280** (+1 new round-22 test), warnings 3
baseline, binary md5 `9932fc1f` installed. Rounds 20–21 + picker-UX
follow-ups remain committed (`fabbdeb`, `725633f`, `52fd4db`,
`0153432`, `901c17c`) — all host-validated and user-confirmed;
**nothing else for you to implement**.

---
## Previous — rounds 20/21 CLOSED host-side

Date: 2026-08-09 · Branch: `working`

## Round 20 — done (container commit `fabbdeb`, host-validated)

Your round-20 implementation was pulled and host-validated
(`cargo test --release` 1273/1273 host-side too). The user's label
correction ("Download & Play", not "Downloads & Play") was applied
host-side as `725633f`.

## Round 21 — implemented HOST-SIDE, do not re-implement

The user gave round-21 feedback directly to the host (not via a
FEEDBACK file): the `[Torrent]` popup options are now

- single-file torrent: **Stream / Download / Cancel**
- multi-file torrent: **Stream all / Download all / Select files… / Cancel**

`Stream` = play now; `Download` / `Download all` = keep the file(s) in
`~/Downloads/s2udio-downloads` **without playback** (a new download-only
path: `AppEvent::TorrentScannedDownload`, `WorkRequest::DownloadTorrent`
fallback, multi-file `TorrentDownload.files`). Host-validated
**1275/1275**, warnings 3 baseline, binary md5 `79f722b3` installed,
committed on `working` (host round-21 commit). Nothing for you to do —
just pull `working` and stay aligned.

---
## Previous — round 18 (historical, CLOSED)
# Notes for the container agent — rounds 20/21 CLOSED host-side

Date: 2026-08-09 · Branch: `working`

## Round 20 — done (container commit `fabbdeb`, host-validated)

Your round-20 implementation was pulled and host-validated
(`cargo test --release` 1273/1273 host-side too). The user's label
correction ("Download & Play", not "Downloads & Play") was applied
host-side as `725633f`.

## Round 21 — implemented HOST-SIDE, do not re-implement

The user gave round-21 feedback directly to the host (not via a
FEEDBACK file): the `[Torrent]` popup options are now

- single-file torrent: **Stream / Download / Cancel**
- multi-file torrent: **Stream all / Download all / Select files… / Cancel**

`Stream` = play now; `Download` / `Download all` = keep the file(s) in
`~/Downloads/s2udio-downloads` **without playback** (a new download-only
path: `AppEvent::TorrentScannedDownload`, `WorkRequest::DownloadTorrent`
fallback, multi-file `TorrentDownload.files`). Host-validated
**1275/1275**, warnings 3 baseline, binary md5 `79f722b3` installed,
committed on `working` (host round-21 commit). Nothing for you to do —
just pull `working` and stay aligned.

---
## Previous — round 18 (historical, CLOSED)
# Notes for the container agent — round 18: torrent scan wait — no deadline, live counter + speeds

Date: 2026-08-08 · Branch: `working`

## Current status — ROUND 18 (feedback for isodev)

**User feedback (2026-08-08) → [FEEDBACK-2026-08-08-13.md](FEEDBACK-2026-08-08-13.md) — READ IT FIRST.** The round-17 scan waits a fixed deadline for a magnet's metainfo (host wired `torrent.max_wait_secs` = 15 as a stopgap). The user wants **no deadline at all**: a wait window with a ticking **counter**, an **esc to cancel** line, and a line showing the **estimated DL speed vs the speed needed for playback without buffering (✓/✗)** — some torrents parse slowly, so the user chooses whether to keep waiting. The metainfo wait must move off the work thread (cancellable, with progress events) so a long wait cannot block album art / yt-dlp / downloads.

## ROUND 17 status (implemented host-side, uncommitted, awaiting this round)

Round 17 (scan-driven `[Torrent]` popup: Loading + engine reuse, Play all / Select files for multi-video torrents, torrent files fill the video queue) is implemented and host-validated **1254/1254** on `working` (uncommitted — the next pull applies it). Host follow-up fixes on the same tree: kitty-graphics fallback (`kitty_graphics_supported` in `src/shared/terminal/mod.rs`), torrent file-picker controls (Space toggles / Enter plays / Esc cancels), and the metainfo stopgap above. **Pull `origin/working` when it is committed; the round-17 uncommitted copy in `~/Projects` should be subsumed/dropped as usual.**

**User feedback (2026-08-08) → [FEEDBACK-2026-08-08-12.md](FEEDBACK-2026-08-08-12.md).** Three asks on torrent/magnet playback:

1. The `[Torrent]` popup section must show **"Loading…"** and **scan the torrent up front** (engine start + add + metainfo wait), then present the play actions from the scan result — reusing the scanned engine (today every play spawns a fresh rqbit).
2. **Multi-video torrents** (season packs) get two new actions: **Play all (N files)** (sequential playlist) and **Select files…** (multi-select modal, name + size, plays the chosen files). Play and Download stays single-file-only this round.
3. Playing a torrent must fill the **Queue tab's Video list** with the torrent's files (one `MpvPlaylistEntry` per file, stream URL per positional index) — the Jellyfin season-play path — with the torrent name as context via a synthetic `ctx.yt_info` entry (keyed by stream URL; **never persisted** — it embeds the rqbit auth token).

**Everything before this is CLOSED and merged into your pull**: rounds 14–16, M2.5 (Play and Download), and the downloads-folder move (`~/Downloads/s2udio-downloads`, MPD tab lists it from disk — MPD cannot play files there, so Downloads files play via mpv; MPD queue/playlist stream-replace keeps the stream for out-of-library files). Host-validated **1239/1239**, binary `aceb2321` installed, **user live-confirmed**. Host pushed `origin/working` (3 commits: `27a50f3` round 16, `9847839` M2.5, `ac4029c` downloads move) — **pull first, it is the new baseline.**

**Container note**: the toolchain is installed now (rust 1.97.1) — the standing "no toolchain in the container" pattern is outdated; run `cargo test --release` in-container before handing off (host re-validates anyway).

## MERGED — the rounds below are historical (host-validated, closed)

## MERGED — 2026-08-06

Round 2 (D/E/F/G) validated live and **merged to `main`** at
`216e864` → commit `48cce17` (fast-forward; `origin/master` HEAD). The
working branch is kept as backup and carries the same tree. See
[FEEDBACK-2026-08-06-2.md](FEEDBACK-2026-08-06-2.md) for the full round
record — **read it before your next patch round**: E was dead code (see
below) and the shim unit-test fakes did not match the real module shapes.

- (D) ReplaceAndPlay race: verified live — re-resolved expired entry gets
  tags + `mpris-art` + TUI art immediately (the `metadata_processed_song`
  queue-update catch-up fired).
- (E) mpDris2 Seek crash: **the container's E fix never ran** — two shim
  guards were always False against the real `/usr/bin/mpDris2`
  (`hasattr(wrapper, "seekid")` — no class-level `seekid`, only
  `__getattr__`; `hasattr(method, "func")` — this dbus-python returns
  plain functions with `_dbus_*` attrs). Host fixed both; Seek/SetPosition
  verified live (exit 0, daemon survives).
- (F) artUrl + (G) tracker name: PASS.
- Test fixes: shim test now hermetic (`ART` temp path) and shape-matched;
  was failing 0/1 on the host (real `mpris-art` file present).

## Feedback

- 2026-08-08 → [FEEDBACK-2026-08-08-12.md](FEEDBACK-2026-08-08-12.md) —
  **round 17 — torrent UX**: (1) the `[Torrent]` popup section shows
  "Loading…" and scans the torrent up front (engine + add + metainfo
  wait), reusing the scanned engine for the play actions; (2) multi-video
  torrents (season packs) get **Play all (N files)** + **Select files…**
  (multi-select modal, name + size); (3) playing a torrent fills the
  Queue tab's Video list with its files (Jellyfin-style playlist, per-file
  stream URLs, torrent name as context via a synthetic yt_info entry —
  never persisted). Baseline: 1239/1239, binary `aceb2321`, user
  live-confirmed. Host pushed origin/working.
- 2026-08-08 → [FEEDBACK-2026-08-07-11.md](FEEDBACK-2026-08-07-11.md) —
  **round 14 — Jellyfin episode metadata mismatch**: while playing
  Jellyfin TV episodes ("Play with MPV") the TUI and MPRIS show the
  next episode's title while mpv plays the selected one (DS9 S04E02:
  mpv "The Visitor", TUI/MPRIS "Hippocratic Oath"; S04E03: mpv
  "Hippocratic Oath", TUI/MPRIS "Indiscretion") — exactly +1 in both
  cases, "sometimes" per the user; mpv's own title is correct →
  s2udio-side current-item/playlist-position handling (season-playlist
  construction in `play_video`, `ctx.mpv` resolution, MPRIS mirrors
  s2udio state). **IMPLEMENTED in the container (2026-08-08)**: root
  cause = `loadfile … replace` splices into the old playlist (stale
  `playlist-pos` misindexes the rotated recorded season → +1); fixed
  by rebuilding mpv's playlist on every switch (`mpv_playlist_clear`)
  and confirming the poll's advance against mpv's live `path`
  (`recorded_entry_for_mpv_pos`) — see Current status + session log.
- 2026-08-07 → [FEEDBACK-2026-08-07-10.md](FEEDBACK-2026-08-07-10.md) —
  **round 12 — lyrics buttons live-check round 2** (round-11 live-check
  findings): hover highlights the label text only (not the `●`/`⭘`
  glyph), `⭘` must persist while held (root cause: the 300 ms
  release-check one-shot fires mid-hold on kitty — gate the fallback on
  `TERMINAL.emulator()`), rename `wrong lyrics` → `hide lyrics` (and
  `show lyrics` when hidden) + one-char right margin + the hidden state
  shows the paused-style info panel. Behavior stays as validated.
- 2026-08-07 → [FEEDBACK-2026-08-07-9.md](FEEDBACK-2026-08-07-9.md) —
  **round 11 — lyrics buttons visual/UX refinements**: remove the
  Artist - Title header, `⭘` pressed-while-held only (no repeat while
  held), cluster `● wrong lyrics | ● fetch lyrics`, collapse to
  `● wrong | ● fetch` when narrow, hover only the label text (not the
  glyph). **DONE**: implemented, host-validated 1205/1205 (host fixed
  3 compile errors + the split_once space bug + 2 style assertions),
  binary `f9d3532e`, live-checked — **closed**.
- 2026-08-07 → [FEEDBACK-2026-08-07-8.md](FEEDBACK-2026-08-07-8.md) —
  **round 10 — lyrics panel buttons: `wrong lyrics` + `fetch lyrics`**
  on the lyrics pane's top row (`src/ui/panes/lyrics.rs`); ●/⭘ pressed
  markers, standard hover; wrong-mark hides the lyrics; fetch overwrites
  the current lyrics and clears the wrong-mark (hidden lyrics reappear).
  Mouse only, no keybinds; keep 1196/1196. **DONE**: host-validated
  1202/1202, binary installed, live-checked — **closed**.
- 2026-08-07 → [FEEDBACK-2026-08-07-7.md](FEEDBACK-2026-08-07-7.md) —
  **round 9 — MPD/Playlists info box max height 15 lines**: the info
  box round 8 gave those tabs is `(h−3)×2/3`, unbounded; cap with
  `.min(15)`, the list takes the remainder. Jellyfin/other tabs
  unchanged. Round 8 closed host-side (1194/1194, 6 test fixes,
  binary `77a5ef0`). **DONE**: host-validated **1196/1196** (0 fixes
  needed — container impl committed as-is), binary installed
  `8cd0da1e`; **live-verified 2026-08-07 (info-box height confirmed by
  the user) — closed.**
- 2026-08-07 → [FEEDBACK-2026-08-07-6.md](FEEDBACK-2026-08-07-6.md) —
  **round 8 — left-pane min-width/hide → Playlists + Jellyfin tabs**:
  apply the MPD tab's `tree_width()` behavior (min 50 chars, hidden
  entirely on TUIs ≤ 120 cols) to the Playlists and Jellyfin tabs'
  left panes (both still use the fixed 30/70 split); shared helper
  preferred. Round 7 closed host-side (1190/1190, 4 test fixes).
  **DONE**: host-validated 1194/1194 (6 test fixes), binary installed
  `77a5ef0`; live checks pending user restart.
- 2026-08-07 → [FEEDBACK-2026-08-07-5.md](FEEDBACK-2026-08-07-5.md) —
  **round 7 — two MPD-tab UI items**: `Enter` opens the right-click
  context menu (parity with Playlists/queue lists); left folder-tree
  pane min width 50 chars, hidden entirely when the TUI is ≤ 120 chars
  wide. No MPRIS / backend work. **DONE**: host-validated 1190/1190
  (4 test fixes), binary installed `cb545ec6`; live checks pending
  user restart.
- 2026-08-07 → host commit (MPRIS scenario-A fix landed): the one-line
  `_orig=_orig` fix from
  [FEEDBACK-2026-08-07-4.md](FEEDBACK-2026-08-07-4.md) is committed on
  `working` — repo == installed `bc2bb231`, scenario A + A→B
  transition re-verified live. Your dev copy carries the identical fix
  uncommitted; **discard it on pull**. No new work requested.
- 2026-08-07 → [FEEDBACK-2026-08-07-4.md](FEEDBACK-2026-08-07-4.md) —
  MPRIS scenario-A ROOT CAUSE: s2u-mpdris2 shim `_orig` late-binding
  (find_cover → SetPosition TypeError → no file:// artUrl). One-line
  fix, hot-verified live (artUrl per track, valid JPEGs). Host
  baseline green (container stale-install was container-only).
  **DONE**: fix committed host-side, repo == installed `bc2bb231`,
  re-verified.
- 2026-08-07 → [FEEDBACK-2026-08-07-3.md](FEEDBACK-2026-08-07-3.md) —
  MPRIS first live run: automated gates PASS; scenario A FAIL (local
  library art missing); baseline flagged stale in container only.
- 2026-08-07 → [FEEDBACK-2026-08-07-2.md](FEEDBACK-2026-08-07-2.md) —
  round-4 host validation: 22 compile/test errors fixed, 3 never-run
  test assertions aligned to spec, 1187/1187; **live UI checks pending
  user restart — do not start new work**.
- 2026-08-07 → [FEEDBACK-2026-08-07.md](FEEDBACK-2026-08-07.md) —
  round 3 validated (1185/1185, scrollbar behavior user-confirmed) +
  round-4 items: settings nav parity, seekbar hover-left, queue
  ctrl+tab seekbar focus w/ interactive seek, context-menu hover.
- 2026-08-06 → [FEEDBACK-2026-08-06-2.md](FEEDBACK-2026-08-06-2.md) —
  round 2 host validation: D/F/G PASS; E fixed host-side (dead-code
  patches); test hermeticity + shape fixes; merged to `main`.
- 2026-08-06 → [FEEDBACK-2026-08-06.md](FEEDBACK-2026-08-06.md) — round 1
  host validation: audio/video MPRIS working; host fixed compile errors,
  Int32 crash, socket discovery, object path; open items D–G (now closed).

## Round status (host validation round 2, 2026-08-06)

- **Verified live**: D (expired `SuicideMixes` entry re-resolves → tags +
  art immediately), F (artUrl without volume change), G (`s2u_running()`
  matches `s2udio`); `cargo test --release` 1181/1181; shim 1/1 (fixed),
  tracker 6/6, s2u-mpdris 21/21.
- **Host-side fixes in this round**: `s2u-mpdris2` — install a real
  `seekid` class method (shadows `__getattr__`/`call()`/`reconnect()` trap,
  CommandError → no-op) and wrap Seek/SetPosition with `_dbus_*`-carrying
  functions; `tests/mpdris_shim/` — hermetic `ART` + official-shape fakes.
- **Remaining open (next rounds)**: nothing from D/E/F/G; stray `~` dir
  in the repo root (trash when convenient). **Filed as non-issue
  (2026-08-06)**: mpDris2 `ExcessNotificationGeneration` burst during
  stream-state churn — cosmetic notification spam only, daemon
  unaffected, not worth a fix.
- **Host cleanup done (2026-08-06)**: `~/Projects/.s2u-yt-fix.root-owned`
  was already trashed (15:13, `~/.trash/20260806-151307-…`; HANDOFF
  Pending updated); stale `REAL_YTDLP` in
  `~/.local/share/s2u-yt/state/manifest` fixed to the pipx venv path
  (wrapper never read it — dormant).

## Standing context

- Saved entry in `SuicideMixes` playlist = resolved googlevideo URL, original
  `https://www.youtube.com/watch?v=8QoopP250vQ`. yt-info cache keyed by both stream
  URL and canonical link; `duration` present on fresh resolves (old entries `None`).
- Environment: binary `~/.local/bin/s2udio` (md5 `f9d3532e`, round-11
  build; running instance restarts to pick it up), scripts
  `s2u-mpv-tracker`/`s2u-mpdris2`/`s2udio-mpris` updated + `mpDris2.service`
  restarted (fixed shim, 21:51); s2u-yt green (bgutil, HTTP 200).
- Test artifact `s2u-yt/CSGO：Browser Edition [xz8tmSUddf8].mp4` trashed
  2026-08-06 (was untracked, do not re-add).
