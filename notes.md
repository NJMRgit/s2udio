# Notes for the container agent — round 34 HOST FIX (2026-08-14) — do not re-implement

## HOST-SIDE FIX on `working` (validated live; committed host-side)

Host pulled `37d2093` (round 34), built + validated, and found ONE live
issue (the unit suite was green — 1407/1407 — but did not cover it):

**The lyrics pane was unfocusable, so the whole edit-mode keyboard was
dead live.** `PaneType::Lyrics` sat in `UNFOSUSABLE_TABS` (both debug
and release lists, upstream-rmpc heritage). Key events go only to the
tab's focused pane, and clicks on a pane only move focus when the pane
is focusable — so clicking the pencil or a word ran the pane's mouse
handler (edit mode toggled, word selected) but focus stayed on the
queue: `←`/`→`/`w`/`s`/`+`/`-`/Enter/`<C-s>`/Esc never reached the
pane. The round-34 pane tests call `handle_action` directly, so they
passed without exercising the focus gate. Fix: remove
`PaneTypeDiscriminants::Lyrics` from both `UNFOSUSABLE_TABS` lists (the
round-34 code comment already claimed "the lyrics pane is focusable" —
it just was not). Regression test
`config::tabs::tests::lyrics_pane_is_focusable` asserts the list
exclusion. Re-validated: **1408/1408**, warnings 3 baseline.

Live-checked with the fixed binary in a tmux session (SGR mouse
injection + raw keys): pencil click focuses the pane and toggles
`✎`/`✏`; paused edit view shows every word with its raw file time in a
dim style; click selects a word; `←`/`→`/`w`/`s` navigate; `-` /
Shift+`+` nudge ±10 ms live; Enter opens the exact-time modal (typed
`00:40.00`, confirmed → marker written + re-indexed in place); `<C-s>`
saves without leaving edit mode; Esc leaves edit mode writing the
changed `<mm:ss.xx>` markers back with the header + `# lrcgen-gap-
align:v1` stamp intact; a second Esc opens Settings. The user's live
`config.ron` got the five round-34 navigation bindings (`<Left>`/`<Right>`
→ Left/Right, `<S-+>` → NudgeUp, `-` → NudgeDown, `<C-s>` → SaveLyrics).
Binary `5b949c33` installed; the running instance needs a restart to
pick it up.

---
# Notes for the container agent — round 34 IMPLEMENTED (isodev, 2026-08-14)

## ROUND 34 — lyrics edit mode: pencil button + pause shows editable per-word timings — committed on `working`

**User feedback (2026-08-14) →
[FEEDBACK-2026-08-14-0.md](FEEDBACK-2026-08-14-0.md).** Implemented in the
container (working tree, awaiting host pull/validation):

1. **Pencil button** (round-34 §1): `LyricsBtn::Edit` + `edit_btn_area`,
   left-aligned at `area.x + 1` on the lyrics button row; `✎` off / `✏`
   while edit mode is ON / `⭘` while held; hover on the glyph only;
   independent of the right-aligned cluster's fit-collapse.
2. **Edit mode** (round-34 §2): `showing_info()` keeps the lyrics while
   `edit_mode && Pause && current_lyrics.is_some()` (`Stop` still shows
   info); paused edit view renders every visible line with each word's raw
   file time in a dim style (new optional `theme.lyrics.edit_timing`;
   default DIM), the selected word highlighted, word hit areas recorded
   for click selection. Playing keeps the normal karaoke view.
3. **Editing** (round-34 §3): `←`/`→` word-to-word (wraps), `w`/`s`/↑/↓
   line-to-line (same column, clamped); `+`/`-` nudge ±10 ms (step choice
   documented — the shift-for-100-ms variant has no distinct key on the
   user's US layout; exact jumps via the popup); Enter opens an
   `InputModal` prefilled with `mm:ss.xx` (confirm writes the single
   marker atomically + re-indexes); `<C-s>` saves in place; Esc leaves
   edit mode saving (consumed, so Settings needs a second Esc).
   Write-back via the new `src/shared/lrc/edit.rs` session: raw text kept
   verbatim, only changed `<mm:ss.xx>` markers rewritten (interpolated
   words promoted on first edit), header + `# lrcgen-gap-align:v1` stamp +
   enhanced format preserved, atomic temp+rename save. Song change leaves
   edit mode (saving); `LyricsIndexed` while editing rebuilds the session
   in place.
4. **New actions**: `LyricsNudgeUp` / `LyricsNudgeDown` / `LyricsSave`
   (CommonAction + File forms + descriptions + reverse remap + remap-keys
   display name + no-op arms in song_list/search). **New default
   navigation bindings** (defaults + `assets/example_config.ron`):
   `<Left>`/`<Right>` → Left/Right, `<S-+>` → NudgeUp, `-` → NudgeDown,
   `<C-s>` → SaveLyrics. **The live `config.ron` (`keybinds: clear: true`)
   needs these five entries added host-side** (like round 31's `<C-a>`).

**Gate: `cargo test --release` 1407/1407** (1389 baseline + 10 edit.rs +
8 pane tests), warnings 3 baseline. Host: pull `working`, add the live
keybinds, build + install, live-check (pencil toggle, paused timings,
nudge + Esc write-back with the stamp intact, exact-time popup, second-Esc
settings). Full details: `docs/design/Sessions/2026-08-14.md`.

---
## MERGED to main (host, 2026-08-14)

Production `master` (`NJMRgit/s2udio`) carried to **`53efdd6`**: round 33
(settings toggle for the mpDris2 desktop notification on track change).
Master validated (cargo check clean, baseline warnings), clean subset
(tests/ stays on `working`). Installed host-side: binary
`~/.local/bin/s2udio` (backup `s2udio.bak-r33-20260814`), shim
`~/.local/bin/s2u-mpdris2` (backup `s2u-mpdris2.bak-r33-20260814`),
`mpDris2.service` restarted; toggle validated live (state file
`~/.cache/s2udio/mpdris2-notify.json`; disabled = 0 popups, enabled = 1
popup on track change; left enabled = default). `working` pushed to
`s2udio-working`. No further action needed from isodev for this round.

---
## MERGED to main (host, 2026-08-13)

Production `master` (`NJMRgit/s2udio`) carried to **`4ff3b3d`**: rounds 31
(multi-select) + 32 (wheel viewport scroll + queue startup) + the round-32
host follow-up (queue wheel block / startup centering / tab-switch
re-centering — see the HOST FIX entry above). Master validated
**1386/1386**, warnings 3 baseline, clean subset (14 paths). `working`
pushed to `s2udio-working` at `0bfae6a`. No further action needed from
isodev for these rounds.

---
# Notes for the container agent — round 32 HOST FIX (2026-08-13) — do not re-implement

## HOST-SIDE FIX on `working` (validated live; committed host-side)

Host pulled `9b1122a` (round 32), built + validated, and found THREE issues
live (the unit suite was green — 1385/1385 — but did not cover them). Host
fixed on `working` and re-validated: **1386/1386**, warnings 3 baseline.

1. **Wheel could NOT scroll the queue past the selection** (user-reported:
   "the selection getting to the top of the viewport stops me from scrolling
   down further"). `VirtualizedTable::render` restored the state through
   `DirState::select(sel, 0)`, which re-applies the scrolloff clamp — every
   render after a viewport-only wheel scroll pulled the offset back the
   moment the selection hit the top (or bottom) row, so the queue could only
   scroll while the highlight stayed visible. The round-32 wheel tests never
   rendered between events, so they missed it (production renders after
   every wheel). Fix: restore the raw selection + scrollbar without the
   clamp (`inner.select_scrolling` + `scrollbar_state.position`).
   Regression test: `wheel_scrolls_the_viewport_past_the_selection_with_renders_between`
   (draws between every wheel event; fails on the old code).
2. **Startup jump re-centered**: the one-shot `select_at_top(playing_idx)`
   ran, then a second `before_show` (startup tab init) hit the else branch
   which re-selected with `usize::MAX` (center) — fresh start showed the
   playing track CENTERED, not at the top. Fix: the re-show branch now
   keeps the user's selection AND scroll position (only re-lands when the
   selection fell out of bounds, e.g. queue reload). Strengthened
   `first_show_jumps_to_the_playing_song_at_the_top` to assert the offset
   is preserved on re-show.
3. **Tab switch re-centered**: same else branch re-centered on EVERY tab
   switch back (wheel scroll position lost). Fixed by (2) — live-checked:
   scroll away, switch tabs, come back, position preserved.

Live-validated on the host: fresh start = playing track first visible row;
wheel scrolls the full list in both directions with the highlight leaving
the window; keyboard moves still scroll the selection back into view
(scrolloff behavior); Settings keeps wheel-moves-selection; MPD/Playlists/
Radio/Search/Help viewport wheel unchanged; Jellyfin still
wheel-moves-selection (excluded per round 32). Binary installed.

---
# Notes for the container agent — round 32 IMPLEMENTED (2026-08-12)

## ROUND 32 — queue startup select-at-top + wheel scrolls the viewport (2026-08-12) — committed on `working`

**User feedback (2026-08-12) →
[FEEDBACK-2026-08-12-5.md](FEEDBACK-2026-08-12-5.md).** Two items:

1. **Queue startup**: on first show, highlight the currently playing track
   and position it as the FIRST visible row via `DirState::select_at_top`
   (offset = playing index, clamped at list end) — one-shot only, later tab
   switches keep the user's selection/scroll. **Already on `working`**
   (commit `2be838c`, 2026-08-11: `startup_jump_done` + `select_at_top` +
   the `first_show_jumps_to_the_playing_song_at_the_top` test) — verified,
   no further code needed.
2. **Wheel scrolls the viewport, not the selection** — implemented across
   Queue (Audio/Video/Chapters), Playlists (root + songs), MPD (tree,
   items, search results), Help and Radio (regions + stations); Settings
   keeps today's wheel behavior.

### Implementation

- New shared widget **`VirtualizedList`** (`src/ui/widgets/virtualized_list.rs`,
  mirroring the queue's `VirtualizedTable`): renders a ratatui `List` from
  its state's offset, showing only the visible slice and shifting the
  selection by -offset — so the highlight only appears when the selected
  row is inside the window. This is required because ratatui's `List`
  render forces the selected item visible, which would undo a viewport-only
  scroll. Also fixes `VirtualizedTable` to not wrongly highlight the
  first/last visible row when the selection sits above/below the window.
- **Viewport scroll helpers**: `DirState::scroll_viewport(dir, amount)`
  (+ `Dir` wrapper) for offset-only scrolling (no selection clamp, clamped
  at the ends), and `virtualized_list::scroll_viewport` /
  `scroll_selection_into_view` for `ListState`-based lists.
- **Wheel arms** in Queue (audio/video/chapters), Playlists, MPD
  (tree/items via the shared tree-browser), Search results, Help, Radio
  now call the viewport-scroll helpers — the highlight stays put and may
  leave the visible area.
- **Keyboard moves keep the old scrolloff behavior**: selection moves
  (w/s/arrows, Home/End, PageUp/Down, cursor-follow syncs, list
  repopulation) scroll the selection back into view explicitly, since the
  virtualized render no longer auto-scrolls. Offsets reset to 0 when lists
  repopulate (MPD/Jellyfin `populate_items`, radio `rebuild_regions` /
  `populate_stations`, etc.).
- **Jellyfin is NOT in the round-32 pane list** (Queue, Playlists, MPD,
  Help, Radio): the shared tree-browser wheel arms branch on a new
  `TreeBrowserCore::wheel_scrolls_viewport()` hook (default true; Jellyfin
  overrides false) so Jellyfin keeps wheel-moves-selection.
- The search *filter* pane (a form of inputs, like Settings) keeps its
  wheel behavior; only the search *results* list scrolls the viewport.

**1385/1385**, warnings 3 baseline. Wheel tests updated to assert
viewport-scroll (offset moves, highlight stays); new tests: playlists list
wheel, MPD items wheel, radio regions+stations wheel, plus
`virtualized_list` unit tests. Host: validate + live-check on `working`.

---
# Notes for the container agent — round 32 FILED (host)

## ROUND 32 — FILED for isodev (2026-08-12) — do not implement host-side

**User feedback (2026-08-12) →
[FEEDBACK-2026-08-12-5.md](FEEDBACK-2026-08-12-5.md).** Two items:

1. **Queue startup**: on first show, highlight the currently playing track
   and position it as the FIRST visible row (scroll offset = playing index,
   clamped at list end) via `DirState::select_at_top` — one-shot only, later
   tab switches keep the user's selection/scroll.
2. **Wheel scrolls the viewport, not the selection** — in Queue, Playlists,
   MPD, Help and Radio (Settings keeps today's wheel behavior). Selection can
   leave the visible area; offset clamps at list/info ends.

Also on `working`: round-30 follow-up `b8552d4` (cava stepper test made
environment-independent — no PipeWire = no-op). Host tree @ `b8552d4`;
**filed only — no code changes host-side**. isodev: implement on `working`;
host validates + live-checks.

---
# Notes for the container agent — round 31 IMPLEMENTED host-side (2026-08-12)

## ROUND 31 — multi-select: Ctrl+A, additive ctrl+click, bulk actions (2026-08-12) — committed on `working`

**User request (2026-08-12) →
[FEEDBACK-2026-08-12-4.md](FEEDBACK-2026-08-12-4.md).** Host implemented
on `working` (round-31 commit `d9fe076`):

1. **Ctrl+A (`CommonAction::SelectAll`, new navigation binding
   `<C-a>`) marks every item of the current list** — queue Audio/Video
   lists, Playlists songs pane, MPD right (Library) pane, Search-mode
   results list. No-op in Jellyfin/Radio/Help/Settings, the MPD folder
   tree, the playlists root list and the search filter column (Esc
   still clears; a second Ctrl+A keeps everything marked).
2. **Ctrl+click is additive** in all five multi-select lists: the row
   under the cursor joins the marks (the initially selected item is
   never dropped) and clicking an already-marked row keeps it — no
   toggle-off (`MarkState::toggle` replaced by `add`/`mark_all`). Plain
   click on another row / Esc still clear.
3. **Marked bulk actions**: playlists song menu acts on every marked
   song (Add/Replace queue, Create/Add playlist, Remove from playlist);
   search results menu gained marked *Add to queue* / *Replace queue*
   (the "all" variants stay); the audio queue menu's *Add to playlist*
   / *Create playlist* act on the marked rows or the highlighted song
   (a single selected song gets them too — round-31 follow-up) and
   renamed *Create audio playlist* → **Create playlist from queue**
   (whole queue, next to the existing *Add queue to playlist*).

The live `~/.config/s2udio/config.ron` navigation table got
`"<C-a>": SelectAll` (the user config has `keybinds: clear: true`, so
the file binding is required). **1378/1378**, warnings 3 baseline,
binary installed (md5 `56617491`) and running in a color kitty (agent
launches must `env -u NO_COLOR` — see the NO_COLOR memory). isodev:
nothing to implement — pull `working` when ready.

# Notes for the container agent — round 30 MERGED to main (2026-08-12)

## ROUND 30 MERGED to main (2026-08-12) — host push

Round 30 (cava is PipeWire-only — FIFO input removed) plus the three
host-validated follow-ups are now **the production `master`**
(`NJMRgit/s2udio` @ `bc6e69f`, clean buildable subset — 15 files):
follow-ups = persist the settings panel's "show virtual devices" toggle
(state.ron, serde-default off), gate every virtual PipeWire node (KDE
split-sink monitors + Easy Effects) with that toggle, and the cava
device row stepper interaction (Enter/d/right focuses the [<] [>]
controls, a/left + d/right cycle, Esc reverts; d/right on any stepper
row enters adjust mode). Host-validated on master: `cargo test
--release` **1367/1367**, warnings 3 baseline. The full tree lives on
this private repo's `working` branch @ `b8b6d35`. Live: the installed
binary is the round-30 build (FIFO gone from the settings panel).

---
# Notes for the container agent — round 30 IMPLEMENTED host-side (2026-08-12)

## ROUND 30 — remove the cava FIFO input (PipeWire only) (2026-08-12) — committed on `working`

**User request (2026-08-12) →
[FEEDBACK-2026-08-12-3.md](FEEDBACK-2026-08-12-3.md).** Remove FIFO from
s2udio's cava — **PipeWire only**. Host implemented on `working`
(round-30 commit): the `CavaInputMethod` enum and the settings panel's
FIFO/PipeWire "cava sample method" toggle are deleted (the generated
cava config always writes `method = pipewire`); the MPD-fifo
sample-format sync (`paste::mpd_fifo_format`) is removed; `setup.sh`
drops the MPD fifo `audio_output` step (renumbered 1/8..8/8); the
container deploy scripts + gates G1/G4/G9 stop requiring the fifo (G9
now checks the deployed config is PipeWire-only and that cava either
runs on the PipeWire input or fails fast with a pipewire error —
headless containers have no PipeWire daemon and Debian/Ubuntu/Fedora
ship cavas without the pipewire input). Old configs still load
(`method: Fifo` ignored, a fifo `source` falls back to `auto`).
**1364/1364**, warnings 3 baseline, mock 62/62, debian-12 container
gates G1/G4/G9 re-validated. isodev: nothing to implement — pull
`working` when ready.

---
# Notes for the container agent — round 29 MERGED to main (2026-08-12)

## ROUND 29 MERGED to main (2026-08-12) — host push

Round 29 (cava `node_name`) is now **the production `master`**
(`NJMRgit/s2udio` @ `fe9f078`, clean buildable subset — one commit on
top of `2c04bcf` carrying `src/config/cava.rs`, `src/shared/paths.rs`,
`src/ui/modals/settings.rs`, `src/ui/panes/cava.rs`,
`scripts/cava-node-name.c` (new), `setup.sh`, `assets/example_config.ron`).
Host-validated on master: `cargo test --release` **1364/1364**, warnings
3 baseline. The full tree lives on this private repo's `working` branch
@ `f17c24b`. Live: s2udio's cava node shows as **`s2udio-cava`** in
pw-dump.

---

# Notes for the container agent — round 29 IMPLEMENTED host-side (2026-08-12)

## ROUND 29 — cava node_name (host live-check feature, 2026-08-12) — committed on `working`

**User request (2026-08-12) →
[FEEDBACK-2026-08-12-2.md](FEEDBACK-2026-08-12-2.md).** Name the cava
PipeWire node s2udio spawns. Investigation: cava hardcodes
`node.name = "cava"` (`pw_stream_new_simple(..., "cava", ...)`), no cava
config/env option exists, and WirePlumber 0.5 cannot rename an app-set
node.name from outside. Host implemented on `working` (round-29 commit):
new `node_name` option (main config + `cava.ron` sidecar; settings panel
preserves it), plus a tiny LD_PRELOAD shim (`scripts/cava-node-name.c`,
built by setup.sh → `~/.local/share/s2udio/libcavaname.so`) that injects
node.name/media.name from `CAVA_NODE_NAME`; `spawn_cava` sets the env
when `node_name` is configured. **1364/1364**, warnings 3 baseline.
Live-verified: s2udio's cava now shows as **`s2udio-cava`** in pw-dump
(plasma's two cavas stay "cava"). isodev: nothing to implement — pull
`working` when ready.

---

# Notes for the container agent — rounds 28 + 28b MERGED to main (2026-08-12)

## ROUNDS 28 + 28b MERGED to main (2026-08-12) — host push

Rounds 28 + 28b are now **the production `master`**
(`NJMRgit/s2udio` @ `2c04bcf`, clean buildable subset — three commits on
top of `898a5bb`: `06b90f7` cava pipewire restart fix (round 25, was
missing from master), `96fc4cb` round 28 (Search tab folded into the MPD
tab), `2c04bcf` round 28b (Shift+Tab toggles Library/Search; Tab/E/Q
cycle tabs again)). The full tree lives on this private repo's `working`
branch @ `5537ad1` (round 28 + round 28b on top of the round-23..27
lineage). Host-validated on master: `cargo test --release`
**1361/1361**, warnings 3 baseline, binary build clean, live-checked by
the user.

---

# Notes for the container agent — round 28b IMPLEMENTED host-side (2026-08-12)

## ROUND 28b — host live-check fix (2026-08-12) — committed on `working`

**Host feedback after validating round 28 →
[FEEDBACK-2026-08-12-1.md](FEEDBACK-2026-08-12-1.md).** The Library/
Search toggle captured `Tab` (and `E`, both NextTab), making the MPD
tab unreachable from the keyboard. User's correction: **ONLY
`Shift+Tab` toggles Library/Search**; `Tab`/`E`/`Q` cycle tabs again.
Host implemented this on `working` (round-28b commit): new global
action `ToggleMpdMode` bound to `<S-Tab>` (defaults + the live
config.ron + example_config.ron); the MPD pane claims it while focused
and toggles — the round-28 NextTab claim is removed; elsewhere it is a
no-op (queue `Shift+Tab` chapters toggle unaffected). **1361/1361**,
warnings 3 baseline. Round 28 itself was host-validated (1360/1360,
live-checked, Search tab removed from the live config.ron, binary
installed). isodev: nothing to implement — pull `working` when ready.

---

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
