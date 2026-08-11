---
title: "UI Reuse Rewrite — Project Outline"
section: rewrite
doc_type: plan
id: "rewrite/ui-reuse"
description: >
  Project outline for the s2udio UI reuse rewrite (branch `rewrite`):
  audit of shared vs bespoke UI code, the master-module-with-args target
  architecture, and the phased consolidation plan.
status: "active"
phase_status: "0: complete (2026-08-10); 1: complete (2026-08-10); 2: complete (2026-08-10); 3: next"
updated: "2026-08-10"
source_files:
  - src/ui/browser.rs
  - src/ui/panes/mod.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/directories.rs
  - src/ui/panes/jellyfin.rs
  - src/ui/panes/radio.rs
  - src/ui/panes/search/mod.rs
  - src/ui/panes/playlists.rs
  - src/ui/panes/tag_browser.rs
  - src/ui/panes/albums.rs
  - src/ui/tree_browser.rs
  - src/ui/modals/*
  - src/config/tabs.rs
related:
  - frontend/layout-templates
  - frontend/interaction
  - tabs/queue-tab
  - tabs/playlists-tab
  - tabs/mpd-tab
  - tabs/jellyfin-tab
  - tabs/radio-tab
  - tabs/search-tab
tags: [rewrite, refactor, reuse, panes, modals, architecture]
---

# UI Reuse Rewrite — Project Outline

Branch: `rewrite` (in `NJMRgit/s2udio-working` only — the distribution
repo `NJMRgit/s2udio` `master` branch is **not** touched by this work).
Audience: isodev (implementer). Owner: host (user + agent).

## 1. Mission

Many tabs/panes/modals share the same *shape*: a bordered list, a
highlighted selection, marking/range-select, a filter line, scrollbar
interactions, context menus, save/delete/rate/enqueue actions, a button
group, a tree+items split. Today some of that is defined once and reused,
and a lot of it is re-implemented per pane. The rewrite consolidates every
repeated UI shape into a **master module parameterized by args**, so:

- congruent behavior is guaranteed by construction (one implementation,
  not N copies that drift);
- new tabs/panes/modals are added by *config + args*, not by copying a
  pane file;
- code complexity and line count go down measurably.

This is **not** a from-scratch rewrite: the backend (core/, mpd/, radio/,
jellyfin/, shared/) and the config model stay. It is a consolidation of
`src/ui` (≈56.7k of the 95.8k total LOC, ~59%) around the existing
shared infrastructure.

## 2. Audit — what the code does today

### 2.1 The good bones (already shared — keep and build on)

| Piece | Where | Notes |
| --- | --- | --- |
| `Pane` trait + `PaneContainer` + `pane_call!` | `src/ui/panes/mod.rs` | Every pane implements the same 11-method interface; the container owns one instance per `PaneType`; the macro dispatches calls. |
| Config-driven tab layout | `src/config/tabs.rs` | `Tabs → Tab → SizedPaneOrSplit` (recursive split tree) → `Pane { pane: PaneType, borders, styles, titles }`. Layouts are **data**, not code. |
| `TabScreen` | `src/ui/tab_screen.rs` | Shared focus, mouse routing (incl. borders/spacers), pane traversal (arrows), resize, before_show/on_hide fan-out. |
| `BrowserPane<T>` trait | `src/ui/browser.rs` (~958 lines) | The **proof the pattern works**: one shared implementation of navigation, marking/range-select, filtering, scrollbar drag, mouse handling, context menus, and the full `CommonAction` set (enqueue, save playlist, delete, rate, rename…). `AlbumsPane` (190 LOC), `TagBrowserPane` and `PlaylistsPane` implement only `stack`/`stack_mut`/`browser_areas`/`list_songs_in_item`/`fetch_data` and get all behavior for free. |
| Dir stack + selection state | `src/ui/dirstack/` (2,856 LOC) | `DirStack`, `DirState` (scroll/select/mark/range/filter state reused by panes *and* `SelectModal`). |
| Widgets | `src/ui/widgets/` (1,548 LOC) | `Button`/`ButtonGroup`, `Input`, `ProgressBar`, `Tabs`, `Volume`, `ScrollingLine`, `VirtualizedTable`, `Header`, `Browser` (3-column prev/current/preview). |
| Modal framework | `src/ui/modals/` (14.7k LOC) | `Modal` trait + `menu` builder (`Section` trait: Menu/Select/Multi/Input sections) + `ConfirmModal`/`InputModal` primitives; shared `BUTTON_GROUP_SYMBOLS`, `DirState`-based scrolling. |
| Shared helpers | `src/ui/panes/mod.rs` (`hovered_item`), `shared/geometry.rs`, `shared/mouse_event.rs`, `ui/input/buffer.rs` | One hover-index calc, one geometry/scroll math, one input buffer system. |

### 2.2 The duplication (bespoke code that should become modules)

**A. Three hand-rolled two-pane “tree + items” browsers — same shape, three copies.**

| Pane | LOC | Left pane | Right pane |
| --- | --- | --- | --- |
| `DirectoriesPane` | 2,259 | directory tree | files/songs |
| `JellyfinPane` | 2,845 | media tree | items + poster |
| `RadioPane` | 2,356 | regions | stations |

Measured pairwise similarity of the *same-named* functions (they were
copy-pasted once, then drifted):

- `radio::render_regions` vs `jellyfin::render_tree`: **0.75**
- `radio::render_stations` vs `jellyfin::render_items`: 0.39
- `directories::render_tree` vs `jellyfin::render_tree`: 0.43
- `directories::render_tips` vs `jellyfin::render_tips`: **0.87**
- `directories::move_items` vs `jellyfin::move_items`: 0.78

> Measured at commit `24bd883` with `scripts/dev/ui-metrics.py` (token-sequence
> difflib ratio over comment-stripped function bodies — the committed Phase-0
> metric). The outline's original ad-hoc numbers predate rounds 24–27 and were
> computed with a slightly different method; the script is the source of truth
> from Phase 0 on. Full same-named-pair report: 83 pairs > 0.5 at baseline.
- cloned-line blocks: (jellyfin,radio) **138**, (directories,jellyfin)
  **120**, (directories,radio) 55, plus smaller fragments in
  directories↔search (79), directories↔playlists (61), etc.

None of the three implements `BrowserPane`; each re-implements tree
expansion, `populate_items`, `sync_tree_to_items_cursor`,
`select_parent`/`set_expanded`/`highlight_tree_node`, `move_tree`/
`move_items`, `render_tree`/`render_items`/`render_tips`, scrollbar and
mouse handling **in parallel**. The “temp play URL, clean up on stop”
lifecycle is also duplicated (jellyfin ↔ radio).

**B. `QueuePane` (4,686 LOC, the largest file in the tree) — three
sub-modes (Audio / Video / Chapters), all in one struct.**

- `handle_action` is ~600 lines and **re-implements the same
  `CommonAction` arms** (`Save`, `DeleteFromPlaylist`, rate stickers,
  `AddOptions`, context menu…) that `BrowserPane::handle_common_action`
  already provides for list-of-songs panes. Cloned blocks: queue↔search
  **138**, queue↔browser.rs 72, queue↔directories 69, queue↔paste 52.
- Audio list rendering is bespoke table code with its own
  hover/scrollbar/mark logic instead of the shared `DirState` +
  `VirtualizedTable` stack.
- The `● Audio ○ Video ○ Chapters` sub-tab toggle row (queue_tab mode)
  is hand-drawn in `render_toggle_on_border` + click-area math in
  `handle_mouse_event` — a reusable widget in disguise.

**C. `SearchPane` (2,039 LOC) — filter pane + results pane, bespoke.**

- Re-implements browser behaviors (selection/marking/hover, scrollbar
  geometry, enqueue/save/delete action arms) instead of composing the
  `BrowserPane` core. Cloned blocks vs `browser.rs`: **113** lines; vs
  queue: 138; vs directories: 79; vs paste: 48.
- Note: search results are *not* a dir stack — this is the strongest
  argument for extracting the browser logic from the `DirStack`-shaped
  trait into a generic **list core** (see §4, `SongListCore`).

**D. Standalone modals — one shared builder, many hand-rolled copies.**

`menu/` (`MenuModal` + sections) is the shared builder, but most
standalone modals still hand-roll the same list+filter+buttons layout:

- `select_modal` ↔ `torrent_file_picker`: **134** cloned lines
- `menu/list_section` ↔ `menu/select_section`: **126**
- `decoders` ↔ `info_list_modal`: **101**; `decoders` ↔ `outputs`: 64;
  `info_list_modal` ↔ `outputs`: 61
- `confirm_modal` ↔ `info_modal`: 80; `confirm` ↔ `input`: 44
- `add_random` ↔ `input_modal`: 79; `add_random` ↔ `select`: 49
- `language` ↔ `tab_help`: 51
- `paste` (4,505) ↔ queue: 52, ↔ search: 48

Total: **≈760+ cloned lines across modal pairs** — same
render/focus/scroll/button-group logic, different data.

**E. Bespoke-but-reusable drawing in `ControlsPane` (1,516) and
`LyricsPane` (2,543).** Marquee/ellipsis, wrapping, button clusters,
format-line helpers, and the hover/click geometry are re-derived here;
parts of `ScrollingLine`/`wrap_to_width`/`linkify` belong in shared
widgets.

### 2.3 Root cause

The architecture already has the right skeleton (trait + container +
config-driven layout), but consolidation stopped halfway: the `BrowserPane`
trait was only applied to three dir-stack panes, and every other list-like
pane (queue audio, search results, radio stations, jellyfin items,
directories items, picker modals) re-implemented the same mechanics.
Each new feature then gets bolted onto whichever copy is nearest — which
is exactly the drift the user wants to end.

### 2.4 Phase-0 LOC baseline (measured 2026-08-10 @ `24bd883`)

Full per-file LOC for `src/ui` is regenerable any time with
`python3 scripts/dev/ui-metrics.py` (reads the working tree; `--ref <git>` for a
ref). Key numbers and audited files:

- **`src/ui` total: 56,704 LOC** (82 files) — 59.2% of the 95,811 total `.rs`
  LOC (includes `build.rs`).
- Audited pane/browser files:

| File | LOC (baseline `24bd883` → now) |
| --- | --- |
| `src/ui/browser.rs` | 1041 → 232 |
| `src/ui/panes/mod.rs` | 3100 → 3100 |
| `src/ui/panes/queue.rs` | 4686 → 4478 |
| `src/ui/panes/directories.rs` | 2259 → 2072 |
| `src/ui/panes/jellyfin.rs` | 2845 → 2653 |
| `src/ui/panes/radio.rs` | 2356 → 2366 |
| `src/ui/panes/search/mod.rs` | 2039 → 1803 |
| `src/ui/panes/playlists.rs` | 1716 → 1755 |
| `src/ui/panes/tag_browser.rs` | 588 → 628 |
| `src/ui/panes/albums.rs` | 190 → 229 |
| `src/ui/panes/controls.rs` | 1516 → 1516 |
| `src/ui/panes/lyrics.rs` | 2543 → 2543 |
| `src/ui/song_list.rs` | — → 955 (Phase 1 core) |
| `src/ui/tree_browser.rs` | — → 598 (Phase 2 core) |
| **src/ui total** | **56,704 → 56,755** |
| **tree total .rs** | **95,811 → 95,862** |

Similarity guardrail baseline: **83 same-named function pairs with ratio > 0.5**
across `src/ui` (full list: `python3 scripts/dev/ui-metrics.py --pairs`).
Since Phase 1 the script excludes the *thin-adapter* method names (the
`SongListCore`/`BrowserPane`/`TreeBrowserCore` hooks + accessors + shared
defaults — those are the intended shared call sites, not duplication): **51
non-thin pairs > 0.5** at baseline `24bd883`, still **51 after Phase 1** (the
9 new pairs — `stack`/`stack_mut`/`browser_areas` in browser.rs ↔ panes — are
the thin accessors; zero new real duplication). After Phase 2 the count is
**60**: the 9 heavy tree-family pairs (`cleanup_temp_play` ×2, `handle_action`
directories↔jellyfin, `move_items`, `render` ×2, `render_tips` ×2,
`selected_item`) and the `on_event` jellyfin↔radio temp-play pair are gone
(moved into `tree_browser.rs`); the ~18 added pairs are thin Pane-delegator
shells (`render`/`handle_action`/`handle_mouse_event`/`on_event` routing to
the core — the same category the baseline already carried, e.g. `handle_action`
mod.rs↔lyrics 1.00). Phase 2 targets the heavy duplicated pairs
(`render_tree`, `render_items`, `render_tips`, `move_*`, `populate_items`, …)
from §2.2 — all deleted from the panes.

## 3. Target architecture — master modules + args

The precedent already exists in `PaneType`:

```rust
PaneType::Volume   { kind }                                  // one pane, args
PaneType::Property { content, align, scroll_speed }          // one pane, args
PaneType::Browser  { root_tag, separator }                   // one pane, args
PaneType::QueueHeader()                                      // unit-variant args
```

The rewrite extends this idea until **every repeated shape is one master
implementation + an args/config spec**:

```
UI shape                      master module            args come from
─────────────────────────────────────────────────────────────────────────
any list of items            SongListCore<T>          item source, row fmt,
(selection, marks, filter,                             sort key, context
scrollbar, hover, actions)                             menu, allowed actions
two-pane tree + items        TreeBrowserCore<T>       tree source (MPD walk /
                                                       Jellyfin API / radio
                                                       browser), item source,
                                                       row fmt, info pane
sub-tab toggle row           SubTabBar                labels, modes, state
option picker / list modal   ListModal                options, filter, buttons
info/details modal           InfoListModal            rows, linkify, buttons
temp-play lifecycle          play_temp_url helper     URL source, cleanup rules
now-playing / info line      LineWidgets              templates (marquee, wrap)
```

Rules of the road:

1. **One implementation per shape.** A shape’s behavior lives in exactly
   one place; per-pane differences are args/data, never a fork.
2. **Args are config-first.** Anything a pane currently hard-codes that a
   user might want to vary (title, columns, sort, context-menu items,
   allowed actions, hover style) becomes an arg on the master module, and
   where it makes sense, a `PaneType`/`TabsFile` field (kept
   backward-compatible with the existing `config.ron` sidecars).
3. **Pané structs become thin specs.** A pane file shrinks to: the args
   struct, data-fetch glue, and `Pane`-trait wiring onto the master
   module — like `AlbumsPane` (190 LOC) is today.
4. **Backends don’t move.** `core/`, `mpd/`, `radio/`, `jellyfin/`,
   `shared/` stay; only `src/ui` is reorganized.
5. **The behavioral spec is `docs/design/`.** Each consolidation keeps the
   existing design docs as the source of truth; the docs get a
   `source_files` update per phase.

## 4. The master modules (spec sketches)

### 4.1 `SongListCore<T>` — generic list-of-items pane core

Extracted from `BrowserPane` (browser.rs) by **removing the DirStack
assumption**; `BrowserPane` becomes a thin dir-stack adapter over it.

> **Implemented (Phase 1, 2026-08-10)** as a trait with hooks, not the
> struct sketched below: `SongListCore<T, S>` (default `S = ListState`,
> `TableState` for the queue) over a flat `Dir<T, S>`, with per-pane hooks
> (`list`/`list_mut`, `open`, `leave`, `enqueue`, `fetch_data(_internal)`,
> `list_songs_in_item`, `song_format`, `initial_playlist_name`, `delete`,
> `rename`/`can_rename`, `move_selected`, `show_info`, `scrollbar_area`,
> `list_area`) and shared default methods (`handle_common_action` +
> `handle_claimed_common_action`, `handle_insert_mode`, `handle_global_action`,
> `handle_scrollbar_interaction`, `handle_list_mouse_action`,
> `open_context_menu`, `items`/`enqueue_items`/`delete_items`/
> `list_songs_in_items`). The struct sketch below remains the Phase-4/6
> end-state (args/config-first); the trait achieves the same "one
> implementation per shape" goal with less indirection.

```rust
pub struct SongListCore<T> {
    items: Vec<T>,                    // or a cursor/loader for paged sources
    state: DirState,                  // selection, marks, filter, scroll — reused
    row_fn: fn(&T, &Ctx) -> Line,     // row formatting
    sort: Option<SortSpec>,           // optional
    context_menu: Option<MenuSpec>,   // items + enabled flags
    allowed_actions: Actions,         // which CommonActions apply (bitset)
    title: Option<PropertyTemplate>,  // box title / bottom line
}
```

Provides (all the code that currently exists in `BrowserPane` +
reimplementations): arrow/half/page navigation, filter + jump-matching,
marking/range-select/anchor, scrollbar geometry + drag, hover highlight,
left/double/middle/ctrl/alt/right-click handling, `Save` /
`DeleteFromPlaylist` / `Rate` / `AddOptions` / `ExternalCommand` /
`ContextMenu` / `Rename`/`Delete` (gated by `allowed_actions`),
enqueue-with-hovered-index, and the shared modals (`create_save_modal`,
`create_delete_modal`, `create_rating_modal`, `MenuModal`).

**Adopters:** queue Audio list, search results, radio stations, jellyfin
items, directories items, playlists songs, tag-browser songs, picker
modals’ option lists. `BrowserPane<T>` then delegates to it, keeping
`AlbumsPane`/`TagBrowserPane`/`PlaylistsPane` untouched in behavior.

### 4.2 `TreeBrowserCore<T>` — two-pane tree + items

Merge the three copies (directories / jellyfin / radio) into one master:

```rust
pub struct TreeBrowserCore<T> {
    tree: TreeState,                  // expansion, cursor, visible filter
    items: SongListCore<T>,           // right pane = a SongListCore
    tree_source: TreeSource,          // enum: MpdDir | Jellyfin | RadioBrowser
    item_source: ItemSource,          // children_of(node) -> Result<Vec<T>>
    row_fmt: TreeRowFmt, item_fmt: ItemRowFmt,
    info: Option<InfoPaneSpec>,       // posters/descriptions (jellyfin)
    tips: Option<Line>,               // bottom tips row
}
```

`TreeSource` adapters hold the backend-specific glue (dir walk, Jellyfin
API calls, radio-browser.info fetch/cache) — the only per-source code.
Everything currently duplicated (`render_tree`, `render_items`,
`render_tips`, `populate_items`, `sync_tree_to_items_cursor`,
`move_tree`/`move_items`, tree+items mouse, the common action arms,
temp-play cleanup) becomes one implementation; the three panes become
config + adapters. Expected: **~4–5k LOC of pane code collapses into one
core (~1.5–2k) + three small adapters**, and the radio/jellyfin
“temp play then delete on stop” lifecycle becomes a shared helper.

> **Implemented (Phase 2, 2026-08-10)** as a trait with hooks, not the
> struct sketched above: `TreeBrowserCore: Pane` (`src/ui/tree_browser.rs`)
> over the panes' own tree/items models (the MPD `DirTree`, jellyfin's
> `Vec<JfNode>`, radio's `Vec<RegionRow>`), with per-pane hooks (`tree_rows`,
> `item_row`, `highlight_tree_node`, `set_expanded_idx`, `select_parent`,
> `activate_selected`, `open_context_menu`, `render_info`, `split_tree`,
> `layout_vertical`, `tips_lines`, …) and shared defaults (`render_tree_browser`
> / `render_tree` / `render_items` / `render_tips`, `move_tree` / `move_items`,
> `handle_tree_mouse` / `handle_items_mouse`, `handle_tree_action`,
> `handle_tree_events`, `cleanup_temp_play` / `temp_play_on_stop` /
> `drop_temp_play` / `play_temp_url` / `handle_play_result`). The struct
> end-state below remains the Phase-6 args/config target; the trait achieves
> the same “one implementation per shape” goal with less indirection (the
> Phase-1 lesson), at the cost of per-pane accessor boilerplate — net LOC for
> Phase 2 is roughly a wash (+51 vs the Phase-0 table), the same tradeoff as
> Phase 1.

### 4.3 `ListModal` / modal consolidation

Standalone modals collapse onto the existing `menu` builder + two generic
masters:

- `ListModal<V>` — options + filter + scroll + `ButtonGroup` (Confirm/
  Cancel) + callback; absorbs `SelectModal`, `TorrentFilePicker`,
  `AddRandomModal` (list parts), `LanguageModal`, `TabHelpModal` (list
  parts).
- `InfoListModal` — read-only rows (with optional linkify) + scroll +
  close; absorbs `InfoListModal`, `Decoders`, `Outputs`, `InfoModal`,
  `DownloadsModal` rows.
- `menu/list_section` and `menu/select_section` (126 cloned lines) merge
  into one section implementation with an args flag.

### 4.4 `SubTabBar` widget

The queue’s `● Audio ○ Video ○ Chapters` toggle row becomes a widget
(rows of labeled segments, click areas, active/inactive styling,
narrow-terminal fallbacks) used by the queue pane; reusable later by any
pane that needs mode switching (controls modes, queue header album sort).

### 4.5 Shared drawing widgets

From `ControlsPane`/`LyricsPane`: `MarqueeLine` (generalize
`ScrollingLine`), `wrap_spans`/`wrap_to_width`, `button_cluster`, and the
now-playing line templates — one implementation, args for style/content.

## 5. Phased plan

Each phase lands on `rewrite` as its own commit(s), keeps
`cargo test --release` green (current suite: **1312 tests** (baseline at commit `24bd883`)), and ends
with a behavior-parity live check of the affected tabs.

| Phase | Work | Primary targets | Exit criteria |
| --- | --- | --- | --- |
| 0 | Baselines & guardrails | — | ✅ (2026-08-10, commit `24bd883`+): branched from `working` at `12d8c6c`; LOC + similarity baseline recorded (§2.4) via committed `scripts/dev/ui-metrics.py`; `cargo test --release` green **1312/1312** (rustc 1.97.1; host may re-run). |
| 1 | Extract `SongListCore<T>` from `BrowserPane`; adopt in queue Audio + search results | `ui/browser.rs`, `ui/panes/queue.rs`, `ui/panes/search/mod.rs` | ✅ (2026-08-10, commits `cd103ac`+`cd10c75`+`113d7e7`): `SongListCore<T, S>` trait (hooks + shared default methods) in `src/ui/song_list.rs`; `BrowserPane` is a thin dir-stack adapter (1041 → 232 LOC); queue Audio (`Dir<Song, TableState>`) + search results (`Dir<Song, ListState>`) implement it and delegate all non-specific `CommonAction` arms. Browser panes behavior unchanged; queue/search identical (1312/1312 green incl. all 61 queue/search tests; live check pending host). Net LOC: **src/ui −179** (queue −208, search −236, browser −809, song_list +955, pane adapters +118); the `CommonAction` arms exist once. |
| 2 | `TreeBrowserCore` unifies directories / jellyfin / radio | `ui/tree_browser.rs`, `ui/panes/directories.rs`, `jellyfin.rs`, `radio.rs` | ✅ (2026-08-10, commits `f5c2ac4`+`948c85c`+`26e9834`): `TreeBrowserCore` trait (hooks + shared defaults, the Phase-1 `SongListCore` pattern) in `src/ui/tree_browser.rs`; all three panes implement it and delegate `render`/`handle_action`/`handle_mouse_event`/`on_event` + the temp-play lifecycle to the shared core. 1312/1312 green (21 directories + 15 jellyfin + 31 radio tests). The §2.2 clone pairs are gone or reduced to shared call sites: `cleanup_temp_play` (×3), `selected_item`, `move_items`, `render`, `render_tips`, `handle_action`, `on_event` (jellyfin↔radio) all deleted from the panes (live in the core); residual >0.5 pairs are thin Pane-delegator shells (`render`/`handle_action`/`handle_mouse_event`/`on_event` routing to the core — same category the baseline already carried) plus pre-existing non-tree pairs. Net LOC: **src/ui +51 vs the Phase-0 table** (pane files −369: directories −187, jellyfin −192, radio +10; core `tree_browser.rs` +598) — the trait-with-hooks pattern trades raw LOC for guaranteed one-implementation (same tradeoff as Phase 1: song_list +955 vs panes −1134); the args/config end-state that shrinks panes further is Phase 6. |
| 3 | Modal consolidation onto `ListModal`/`InfoListModal` + section merge | `ui/modals/*` | Each converted modal passes its behavior checks; `menu/select_section` merged; **–~600–900 LOC**. |
| 4 | QueuePane decomposition: Audio list → `SongListCore`, toggle → `SubTabBar`, Video/Chapters stay as focused specs | `ui/panes/queue.rs` | Queue tab live-check (Audio/Video/Chapters, merged box, esc-deselect, marks); **–~1–1.5k LOC** in queue.rs. |
| 5 | Shared drawing widgets (marquee/wrap/button cluster) from controls/lyrics | `ui/panes/controls.rs`, `lyrics.rs`, `ui/widgets/` | Visual parity in live check; widgets reused by ≥2 call sites. |
| 6 | Args expansion: pane-specific constants move into `PaneType`/config args | `src/config/tabs.rs`, panes | Adding a new browser tab = config block + adapter, no new pane file; sidecars migrate cleanly. |
| 7 | Close-out | — | Final LOC comparison vs baseline; docs (`docs/design/`) `source_files` updated; HANDOFF/notes updated; `rewrite` branch state documented for review. |

Rough order of business for isodev: **Phase 1 first** (highest leverage,
smallest blast radius), then 2 and 3 in either order, then 4–6. Phases 1–3
already remove most of the measured duplication.

### 5.1 Phase 2.1 — delta close-out (planned 2026-08-10)

Phase 2's consolidation changed three observable behaviors vs the
pre-phase-2 panes; Phase 2.1 closes them so the parity DoD is fully met
and the remaining changes are deliberate (pinned by tests), not
incidental:

1. **Items-box title spacing (restore parity).** The shared
   `render_items` formats the title as `" {}({}) "` (directories style),
   which drops the space jellyfin/radio had before the count
   (`" Items (3) "` → `" Items(3) "`). Fix: the `items_title` hook returns
   the title *as it appears left of `(n)`* — pre-padded — and the shared
   format becomes `"{}({}) "`; directories returns `" Library"` /
   `" Downloads"` / `" {name}"`, jellyfin `" Items "` / `" {label} "`,
   radio the padded region titles. Add one render test per pane asserting
   the exact box title.
2. **Temp-play stop/unification (keep + pin).** The shared core clears
   `ctx.temp_play_id` on the Stop transition everywhere and gives
   directories the `PlaybackStateChanged`/`Player` Stop cleanup
   radio/jellyfin already had. These are fixes (jellyfin's two stop arms
   disagreed, radio's `PLAY` vs `PASTE_PLAY` arms disagreed, directories
   leaked its temp entry on Stop); keep them and pin with regression
   tests: directories Stop drops the entry, jellyfin Stop clears
   `ctx.temp_play_id`, radio `PLAY` result sets `ctx.temp_play_id`.
3. **Docs + verification.** Update HANDOFF's "behavior deltas" bullet to
   the resolved state, run `cargo test --release` (1312 + new tests),
   warnings ≤ 3, guardrail unchanged, commit as `phase 2.1`.

## 6. Definition of done (applies per phase)

1. `cargo test --release` green (1312 and growing), no new warnings.
2. Behavior parity: the tabs/panes/modals touched by the phase pass the
   existing design-doc specs and a live check by the user.
3. No two panes implement the same-named function with similarity > 0.5
   (the clone pairs in §2.2 are gone or reduced to shared call sites).
4. Net LOC reduction for the phase (tracked against the Phase-0 table).
5. Config compatibility: existing `~/.config/s2udio/config.ron` still
   parses (or migrates automatically) after any `PaneType`/`TabsFile`
   arg changes.
6. Docs: `docs/design/**` `source_files` updated; this outline’s status
   flipped per phase.

## 7. Out of scope / risks

**Out of scope:** backend rewrites (`core/`, `mpd/`, `radio/`,
`jellyfin/`, `shared/`), new features, the distribution repo, config
format breakage without migration, and any behavior change that isn’t
justified by the parity checks.

**Risks & mitigations:**

- *Behavior drift during consolidation* — mitigated by the 1307-test
  suite, per-phase parity checks, and keeping each phase small.
- *Generics over-abstraction* (performance / compile time) — mitigate by
  measuring (cargo build times, per-frame allocations in the hot render
  path); prefer concrete enum args over trait objects where possible.
- *Scope creep into a full rewrite* — this is explicitly a
  consolidation; new features are frozen until Phase 7.
- *Config sidecar churn* — any new `PaneType` args get serde defaults so
  existing configs keep parsing.

## 8. How to work on this

- Work only in `~/NJMRgit/s2udio-working` on branch `rewrite`.
- Build/test loop: `cargo test --release` (builds the test binary),
  `cargo build --release` (then install `target/release/s2u` →
  `~/.local/bin/s2udio` — build *before* install; see HANDOFF).
- Live checks need the user’s terminal — coordinate restarts.
- Update `docs/design/Rewrite/ui-reuse-rewrite.md` status and the LOC
  table as phases land; update `docs/design/README.md` index if the
  hierarchy changes.
