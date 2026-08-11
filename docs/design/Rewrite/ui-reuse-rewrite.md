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
phase_status: "0: complete (2026-08-10); 1: complete (2026-08-10); 2: complete (2026-08-10); 2.1: complete (2026-08-10); 3: complete (2026-08-10); 4a: complete (2026-08-11); 4b: complete (2026-08-11); 5: complete (2026-08-11); 6: complete (2026-08-11); 7: complete (2026-08-11)"
updated: "2026-08-11"
source_files:
  - src/ui/browser.rs
  - src/ui/panes/mod.rs
  - src/ui/panes/queue.rs
  - src/ui/panes/queue/context_menus.rs
  - src/ui/panes/queue/video.rs
  - src/ui/panes/queue/chapters.rs
  - src/ui/panes/directories.rs
  - src/ui/panes/jellyfin.rs
  - src/ui/panes/radio.rs
  - src/ui/panes/search/mod.rs
  - src/ui/panes/playlists.rs
  - src/ui/panes/tag_browser.rs
  - src/ui/panes/albums.rs
  - src/ui/panes/controls.rs
  - src/ui/panes/lyrics.rs
  - src/ui/song_list.rs
  - src/ui/tree_browser.rs
  - src/ui/widgets/marquee.rs
  - src/ui/widgets/wrap.rs
  - src/ui/widgets/sub_tab_bar.rs
  - src/ui/modals/*
  - src/config/tabs.rs
  - scripts/dev/ui-metrics.py
related:
  - rewrite/phase4b
  - rewrite/phase5
  - rewrite/phase6
  - rewrite/phase7
  - rewrite/new-browser-tab
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
  not N copies that drift) — **predictable behavior**;
- new tabs/panes/modals are added by *config + args*, not by copying a
  pane file — **extensibility**;
- code complexity and line count go down measurably.

> **User priority (2026-08-10):** the *aim* is extensibility and
> predictable behavior; LOC reduction is a useful proxy, not a gate.
> Phases may land LOC-neutral or slightly positive when the shared-core
> cost buys one-implementation-by-construction (Phase 2 +51, Phase 3 −55
> vs its −600–900 estimate); the consumer-side paydown is Phase 6.

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

### 2.4 Phase-0 LOC baseline and FINAL comparison (measured `24bd883` vs `HEAD`)

Full per-file LOC for `src/ui` is regenerable any time with
`python3 scripts/dev/ui-metrics.py` (reads the working tree; `--ref <git>` for a
ref — the file list AND the contents come from the ref, so always diff
committed refs). The Phase-0 baseline was measured 2026-08-10 at `24bd883`;
the final comparison (Phase 7, 2026-08-11) is `24bd883` vs `HEAD`. Key
numbers and audited files:

- **`src/ui` total: 56,704 → 57,318 LOC** (82 → 90 files) — 59.2% of the
  95,811 total `.rs` LOC at baseline (includes `build.rs`); **+614 LOC
  overall** (see the per-phase table in §5.6).
- **tree total `.rs`: 95,811 → 96,797 (+986)** — includes `src/config`
  (the Phase-6 `TreeBrowserArgs` serde machinery, `tabs.rs` 1277 → 1672).
- Audited pane/browser/modal files:

| File | LOC (baseline `24bd883` → `HEAD`, final) |
| --- | --- |
| `src/ui/browser.rs` | 1041 → 232 |
| `src/ui/panes/mod.rs` | 3100 → 3100 |
| `src/ui/panes/queue.rs` | 4686 → 3485 |
| `src/ui/panes/queue/context_menus.rs` | — → 347 (4b submodule) |
| `src/ui/panes/queue/video.rs` | — → 373 (4b submodule) |
| `src/ui/panes/queue/chapters.rs` | — → 325 (4b submodule) |
| `src/ui/panes/directories.rs` | 2259 → 2155 |
| `src/ui/panes/jellyfin.rs` | 2845 → 2736 |
| `src/ui/panes/radio.rs` | 2356 → 2467 |
| `src/ui/panes/search/mod.rs` | 2039 → 1803 |
| `src/ui/panes/playlists.rs` | 1716 → 1759 |
| `src/ui/panes/tag_browser.rs` | 588 → 628 |
| `src/ui/panes/albums.rs` | 190 → 229 |
| `src/ui/panes/controls.rs` | 1516 → 1319 |
| `src/ui/panes/lyrics.rs` | 2543 → 2475 |
| `src/ui/song_list.rs` | — → 955 (Phase 1 core) |
| `src/ui/tree_browser.rs` | — → 610 (Phase 2 core; +8 Phase 2.1/6) |
| `src/ui/modals/list_modal.rs` | — → 765 (Phase 3 core; +tests) |
| `src/ui/modals/select_modal.rs` | 317 → 90 (Phase 3 thin adapter) |
| `src/ui/modals/torrent_file_picker.rs` | 501 → 141 (Phase 3 thin adapter) |
| `src/ui/modals/info_list_modal.rs` | 330 → 453 (Phase 3 core; +tests) |
| `src/ui/modals/decoders.rs` | 248 → 67 (Phase 3 thin adapter) |
| `src/ui/modals/menu/list_section.rs` | 296 → 349 (select_section merged in) |
| `src/ui/modals/menu/select_section.rs` | 210 → deleted (merged into list_section) |
| `src/ui/modals/menu/mod.rs` | 649 → 638 (Select dispatch arms gone) |
| `src/ui/modals/menu/modal.rs` | 615 → 601 (select_section builder → list_section) |
| `src/ui/modals/outputs.rs` | 258 → 258 (kept — see §5.2) |
| `src/ui/widgets/marquee.rs` | — → 282 (Phase 5 core: carousel cycle + marquee) |
| `src/ui/widgets/wrap.rs` | — → 84 (Phase 5 core: wrap helpers) |
| `src/ui/widgets/sub_tab_bar.rs` | — → 149 (Phase 4a core: `● Audio ○ Video ○ Chapters`) |
| `src/config/tabs.rs` | 1277 → 1672 (Phase 6: `TreeBrowserArgs` + manual serde; see §5.5) |
| **src/ui total** | **56,704 → 57,318 (+614)** |
| **tree total .rs** | **95,811 → 96,797 (+986)** |

Similarity guardrail (FINAL, measured with the current
`scripts/dev/ui-metrics.py` at both refs): **83 same-named function pairs
with ratio > 0.5** across `src/ui` at baseline `24bd883` (incl. thin
adapters; full list: `python3 scripts/dev/ui-metrics.py --pairs`). The
script excludes the *thin-adapter* method names (the
`SongListCore`/`BrowserPane`/`TreeBrowserCore` hooks + accessors + shared
defaults — the intended shared call sites, not duplication; the list grew
with the phases: Phase 1 added the SongListCore/BrowserPane hooks, Phase 2
the TreeBrowserCore hooks, Phase 6 `tree_args`). **Final counts: 42
non-thin pairs > 0.5 at baseline `24bd883`, 60 at `HEAD`** — the historical
"51 at baseline / 51 after Phase 1" figures in the phase-1 close-out were
measured with the then-current (smaller) thin list; with the FINAL list the
same baseline measures 42 (the five additional excluded names are the
Phase-2 tree-family names `cleanup_temp_play`, `move_items`, `render_tips`,
`select_items_item`, `selected_item` — now shared-core hooks, 9 pairs).
After Phase 2 the count is **60**: the heavy tree-family pairs
(`cleanup_temp_play` ×2, `handle_action` directories↔jellyfin, `move_items`,
`render` ×2, `render_tips` ×2, `selected_item`) and the `on_event`
jellyfin↔radio temp-play pair are gone (moved into `tree_browser.rs`); the
~18 added pairs are thin Pane-delegator shells (`render`/`handle_action`/
`handle_mouse_event`/`on_event` routing to the core — the same category the
baseline already carried, e.g. `handle_action` mod.rs↔lyrics 1.00). The
count is **unchanged at 60 after every later phase** (2.1, 3, 4a, 4b, 5, 6,
7 — identical pair set through HEAD; the Phase-2.1 `items_title`
directories↔radio pair dropped 0.61 → 0.37, and Phase 4b's moved functions
keep unique names so the split adds no new pairs). Final gate: **60
excl-thin pairs at HEAD**.

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

> **Implemented (Phase 3, 2026-08-10)** as one concrete master per shape
> with args, not generics: `ListModal<'a, V>` (`src/ui/modals/list_modal.rs`)
> holds the options list + `DirState` + scrollbar (incl. drag) +
> `ButtonGroup` + the List↔Buttons focus cycle + Confirm/Close/wheel/
> click/double-click handling once, parameterized by `row_fn`, `size_fn`,
> `buttons`/`confirm_buttons`, `multi_select`+`mark_id`,
> `bottom_title`, `list_right_padding`, `wheel_moves_selection` and
> `scrollbar_drag`. `SelectModal` (317 → 90 LOC) and `TorrentFilePicker`
> (501 → 141 LOC) are thin adapters over it (the Phase-1/2 lesson: trait or
> args, never a fork), keeping their public builders so the 14 + 1 call
> sites were untouched. `InfoListModal` (`src/ui/modals/info_list_modal.rs`)
> generalizes to N columns + a `header` arg and absorbs `DecodersModal`
> (248 → 67 LOC); `OutputsModal` stays standalone (fixed-size, in-table
> header, live refresh + toggle — a different shape, see §5.2). The
> `menu/select_section.rs` module was merged into `list_section.rs`
> (select items = `MenuItem` + a section-level `action` callback); the
> `SectionType::Select` variant and its 16 dispatch arms are gone.

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
`cargo test --release` green (current suite: **1326 tests** — 1312 at the
`24bd883` baseline, +14 from Phases 2.1+3), and ends
with a behavior-parity live check of the affected tabs.

| Phase | Work | Primary targets | Exit criteria |
| --- | --- | --- | --- |
| 0 | Baselines & guardrails | — | ✅ (2026-08-10, commit `24bd883`+): branched from `working` at `12d8c6c`; LOC + similarity baseline recorded (§2.4) via committed `scripts/dev/ui-metrics.py`; `cargo test --release` green **1312/1312** (rustc 1.97.1; host may re-run). |
| 1 | Extract `SongListCore<T>` from `BrowserPane`; adopt in queue Audio + search results | `ui/browser.rs`, `ui/panes/queue.rs`, `ui/panes/search/mod.rs` | ✅ (2026-08-10, commits `cd103ac`+`cd10c75`+`113d7e7`): `SongListCore<T, S>` trait (hooks + shared default methods) in `src/ui/song_list.rs`; `BrowserPane` is a thin dir-stack adapter (1041 → 232 LOC); queue Audio (`Dir<Song, TableState>`) + search results (`Dir<Song, ListState>`) implement it and delegate all non-specific `CommonAction` arms. Browser panes behavior unchanged; queue/search identical (1312/1312 green incl. all 61 queue/search tests; live check pending host). Net LOC: **src/ui −179** (queue −208, search −236, browser −809, song_list +955, pane adapters +118); the `CommonAction` arms exist once. |
| 2 | `TreeBrowserCore` unifies directories / jellyfin / radio | `ui/tree_browser.rs`, `ui/panes/directories.rs`, `jellyfin.rs`, `radio.rs` | ✅ (2026-08-10, commits `f5c2ac4`+`948c85c`+`26e9834`): `TreeBrowserCore` trait (hooks + shared defaults, the Phase-1 `SongListCore` pattern) in `src/ui/tree_browser.rs`; all three panes implement it and delegate `render`/`handle_action`/`handle_mouse_event`/`on_event` + the temp-play lifecycle to the shared core. 1312/1312 green (21 directories + 15 jellyfin + 31 radio tests). The §2.2 clone pairs are gone or reduced to shared call sites: `cleanup_temp_play` (×3), `selected_item`, `move_items`, `render`, `render_tips`, `handle_action`, `on_event` (jellyfin↔radio) all deleted from the panes (live in the core); residual >0.5 pairs are thin Pane-delegator shells (`render`/`handle_action`/`handle_mouse_event`/`on_event` routing to the core — same category the baseline already carried) plus pre-existing non-tree pairs. Net LOC: **src/ui +51 vs the Phase-0 table** (pane files −369: directories −187, jellyfin −192, radio +10; core `tree_browser.rs` +598) — the trait-with-hooks pattern trades raw LOC for guaranteed one-implementation (same tradeoff as Phase 1: song_list +955 vs panes −1134); the args/config end-state that shrinks panes further is Phase 6. |
| 3 | Modal consolidation onto `ListModal`/`InfoListModal` + section merge | `ui/modals/*` | ✅ (2026-08-10, commits `a5aac04`+`53b9e90`): `ListModal<'a, V>` master in `src/ui/modals/list_modal.rs` (args: row/size fns, buttons/confirm_buttons, multi-select + mark_id, bottom title, padding, wheel mode, scrollbar drag); `SelectModal` (317 → 90) + `TorrentFilePicker` (501 → 141) are thin adapters with unchanged public builders — all 15 call sites + the paste picker tests untouched. `menu/select_section.rs` merged into `list_section.rs` (value items + section-level `action`), `SectionType::Select` + 16 dispatch arms deleted. `InfoListModal` generalized to N columns + `header` arg and absorbs `DecodersModal` (248 → 67). 1326/1326 green (+8: 4 ListModal + 4 InfoListModal behavior pins incl. the unified click-row mapping); warnings 3 baseline; similarity guardrail unchanged (60 excl-thin — modals are not in `PANE_FILES`). Net LOC: **src/ui −61** (56,923 → 56,862), tree −110 (96,030 → 95,920) — short of the −600–900 target for the same reason Phase 2 was (+51): the master modules (+763 list_modal, +123 info_list incl. tests) cost more than the thinned consumers saved (select −227, torrent −360, decoders −181, select_section −210); the phase trades raw LOC for one-implementation-by-construction, and the args/config end-state that actually shrinks consumers is Phase 6. Deliberate delta + kept-modals rationale in §5.2. |
| 4 | QueuePane decomposition: Audio list → `SongListCore`, toggle → `SubTabBar`, Video/Chapters stay as focused specs | `ui/panes/queue.rs` | Queue tab live-check (Audio/Video/Chapters, merged box, esc-deselect, marks); **–~1–1.5k LOC** in queue.rs. **4a ✅ (`9b46f54`): `SubTabBar` widget. 4b ✅ (`5bf5a18`+`80b4844`+`c81cb2f`+4b4 close-out): queue.rs decomposed into the module root + `queue/context_menus.rs` + `queue/video.rs` + `queue/chapters.rs` (4447 → 3485, production −962; zero test edits, 1328/1328) — close-out §5.3. Host live-check (§8 of the 4b plan) pending.** |
| 5 | Shared drawing widgets (marquee/wrap/button cluster) from controls/lyrics | `ui/panes/controls.rs`, `lyrics.rs`, `ui/widgets/` | ✅ (2026-08-11, `483a73c`+`490c62e`+`2fcb10c`+`ef4863f`+5b5 close-out): `MarqueeLine` (`ui/widgets/marquee.rs`, 282) + wrap helpers (`ui/widgets/wrap.rs`, 84) extracted with the carousel cycle math untouched; controls (2 render sites) + lyrics + jellyfin adopt the marquee widget, lyrics + jellyfin adopt wrap — ≥2 call sites each. Button cluster + now-playing templates: **documented decisions NOT to merge** (§3 — three cluster shapes / two line-template shapes, see §5.4). 1328/1328 after each commit, warnings 3 baseline, guardrail 60 excl-thin identical pair set. Live check (§8 of the plan) pending. |
| 6 | Args expansion: pane-specific constants move into `PaneType`/config args | `src/config/tabs.rs`, panes | ✅ (2026-08-11, `4a5b054`+`a1caf6b`+`9abb201`+6.4 close-out): `TreeBrowserArgs` (50/120/Some(15) serde defaults) on the four browser variants (both enums; custom `Deserialize` keeps bare `Directories` parsing — see §5.5); the four panes + `TreeBrowserCore` read the args (tree min width / hide threshold / info cap; defaults = today's constants); construction pattern documented in `docs/design/Rewrite/new-browser-tab.md` (config block + thin adapter, never a new core; the four adapters stay per-backend — §5.5 decision). 1337/1337, warnings 3, guardrail 60 excl-thin. |
| 7 | Close-out | — | ✅ (2026-08-11, commits 7.1–7.3): FINAL LOC comparison `24bd883` vs `HEAD` (§2.4 + §5.6 per-phase table); docs/design `source_files` sweep; HANDOFF/notes final state; `REVIEW.md` branch-state writeup; session log. 1337/1337, warnings 3, guardrail 60 excl-thin. |

Rough order of business for isodev: **Phase 1 first** (highest leverage,
smallest blast radius), then 2 and 3 in either order, then 4–6. Phases 1–3
already remove most of the measured duplication.

### 5.1 Phase 2.1 — delta close-out ✅ (2026-08-10)

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

**Implemented (2026-08-10)**: `render_items` now formats
`"{}({}) "` and each pane's `items_title` returns the pre-padded title —
directories `" Library"` / `" Downloads"` / `" {name}"`, jellyfin
`" Items "` / `" {label} "`, radio `" Favourites "` / `" Local — closest "`
/ `" {name} "` / `" {state} "` / `" Stations "`. The pre-Phase-2 titles are
restored exactly (`" Library(3) "`, `" Items (3) "`, `" Stations (3) "`).
**1318/1318** green (+6: one render title test per pane — the
directories/jellyfin/radio items-box titles — plus three temp-play pins:
directories Stop drops the temp entry, jellyfin Stop clears
`ctx.temp_play_id`, radio `PLAY` sets `ctx.temp_play_id`); warnings 3
baseline unchanged; similarity guardrail unchanged (60 excl-thin pairs,
identical pair set vs the Phase-2 close-out; the `items_title`
directories↔radio pair dropped 0.61 → 0.37, now well under the
threshold). Net LOC: **src/ui +168** (panes +155 tests/comments,
tree_browser +4), tree total .rs **96,030** — see §2.4 table.

### 5.2 Phase 3 — delta close-out ✅ (2026-08-10)

Phase 3's consolidation changed one observable behavior and deliberately
kept four modals standalone; both are documented so the parity DoD is
explicit:

1. **Unified click-row mapping (fix + pin).** The legacy `SelectModal`
   mouse handler subtracted an extra row (`y - options_area.y - 1`), so
   clicking the second or later option selected the row above it; the
   torrent picker (round 17–22, user-validated) uses the correct
   `y - options_area.y` mapping. The master implements the correct one and
   `click_on_second_row_selects_second_option` pins it.
2. **Kept standalone (not list-shaped).** `OutputsModal` (fixed 70x10
   popup, in-table header, `column_spacing(0)`, live refresh via
   `on_query_finished`/`on_event` + toggle-on-Confirm), `InfoModal`
   (wrapped message box + OK button, no scroll), `DownloadsModal` (live
   updates + per-row context menu), `LanguageModal` and `TabHelpModal`
   (section headers + Enter-applies / Basic-Advanced toggle + Tab
   switching) and `AddRandomModal` (input combobox) — folding them into
   the masters would add size-mode/header-style/spacing/key-hook args
   (a fork, not an arg). Their shared fragments are small
   (language↔tab_help 51, add_random↔input 79 cloned lines) and Phase 5's
   shared-drawing-widgets work is the right home for any further reuse.
3. **Docs + verification.** This section + §2.4 table + §4.3 updated;
   `cargo test --release` **1326/1326** (+8 vs Phase 2.1's 1318), warnings
   3 baseline, similarity guardrail unchanged at 60 excl-thin pairs,
   commits `a5aac04` (3a) and `53b9e90` (3b+3c). Measured on the
   committed refs with `ui-metrics.py --ref` (the script's default
   `loc_table` reads the file list from HEAD and contents from the
   worktree — always diff committed refs). The −600–900 LOC target is not
   met (−61): like Phase 1 (`song_list +955 vs panes −1134`) and Phase 2
   (+51), the master-module pattern front-loads the shared core; the
   duplication it removes is measurable (select↔torrent 134, list↔select
   sections 126, decoders↔info 101 cloned lines + the 16-dispatch-arm
   `SectionType::Select` + dead `zip_longest2`), and the consumer-side
   paydown lands with the Phase-6 args/config end-state.

### 5.3 Phase 4b — queue decomposition close-out ✅ (2026-08-11)

Phase 4b decomposed the largest file in the tree (`src/ui/panes/queue.rs`,
4447 LOC) into a module root + three focused submodules, with **zero
behavior change and zero test edits** — the 1773 test LOC in queue.rs's
five `#[cfg(test)]` mods is byte-identical. Executed per the handoff plan
`docs/design/Rewrite/phase4b-queue-decomposition.md` in four commits on
`rewrite` (each independently green):

| Commit | Move set | queue.rs after |
| --- | --- | --- |
| `5bf5a18` (4b1) | `open_context_menu`, `open_audio_context_menu`, `open_video_context_menu` → `queue/context_menus.rs` (347 LOC) | 4128 |
| `80b4844` (4b2) | `render_video`, `handle_video_action`, `video_load_entry`, `video_remove_entries`, `video_move`, `video_scroll_to`, `video_page`, `video_jump`, `video_play_selected`, `follow_playing_video` → `queue/video.rs` (373 LOC) | 3782 |
| `c81cb2f` (4b3) | `render_chapters`, `handle_chapters_action`, `chapters_move`, `chapters_page`, `chapters_jump`, `chapters_select_current`, `chapters_play_selected`, `chapters_scroll_to`, `seek_to` → `queue/chapters.rs` (325 LOC) | 3485 |
| 4b4 (close-out) | docs + metrics (this section, §2.4, HANDOFF, plan flipped done, session log) | 3485 |

**Real numbers (measured on committed refs with `ui-metrics.py --ref`
`1867964` vs `--ref HEAD` — the script's default reads the file list from
HEAD and contents from the worktree, so new files are silently excluded
unless both sides are committed refs):**

- queue.rs **4447 → 3485 (−962)**: production 2673 → 1711 (**−962**),
  tests 1775 → 1775 (**untouched**). The −~950 production-LOC target is
  met (it is a target, not a gate — the user priority is extensibility +
  predictable behavior).
- New submodules (all `pub(super)` inherent methods on `QueuePane`, so
  every call site in queue.rs, `tab_screen.rs` and the test mods compiled
  unchanged): `queue/context_menus.rs` 347, `queue/video.rs` 373,
  `queue/chapters.rs` 325.
- `cargo test --release` **1328/1328** after each commit; warnings 3
  baseline unchanged (`config/mod.rs` unused-mut, `language.rs` unused ctx,
  `paste.rs` `AddAfterCurrent`); zero edits to the 1773 test LOC.
- Similarity guardrail **unchanged at 60 excl-thin pairs** (identical
  same-named-fn pair set before/after — the moved functions keep unique
  names, so splitting adds no new >0.5 pairs; queue.rs-only pairs
  (`act`, `click`, `songs`, `row_bg`, …) stay as-is because the functions
  still exist, just in child files).
- Net LOC: **src/ui +83** (56,987 → 57,070), tree +83 (96,045 → 96,128)
  — the three submodule headers/imports (`use super::…` blocks + `mod`
  declarations) cost 28 lines per file on average; the split is a pure
  move, so LOC is a wash by design (the paydown that matters — one
  implementation per shape — is the phase-4 queue-audio core work already
  landed in Phase 1, and the remaining consumer paydown is Phase 6).
- `queue.rs` stays a **file** (not `queue/mod.rs`) — the metrics script's
  `PANE_FILES` and `tab_screen.rs`/`panes/mod.rs` paths reference the
  path, and the plan's layout requires it.

**Why this shape is safe (per plan §3):** submodules are children of
`queue`, so they read queue.rs's private items (`Areas`, helpers, fields)
with only `use super::…`; the moved functions are inherent methods on
`QueuePane` in the submodules, so all call sites compile unchanged — with
one visibility nuance: private inherent methods defined in a child module
are not visible from the parent, so the moved functions are declared
`pub(super)` (identical effective visibility to their original private
declarations in `queue`, which covered `queue` + its test mods).

**Remaining:** the phase-4 host live-check (plan §8 — Audio/Video/
Chapters, marks, context menus, toggle, scrollbars) before phase 4 closes;
then phases 5–7.


### 5.4 Phase 5 — shared drawing widgets close-out ✅ (2026-08-11)

Phase 5 extracted the shared drawing machinery from `ControlsPane` /
`LyricsPane` into `src/ui/widgets/`, per the handoff plan
`docs/design/Rewrite/phase5-drawing-widgets.md`, in five commits on
`rewrite` (each independently green, **1328/1328** after every commit,
warnings 3 baseline unchanged, similarity guardrail unchanged at **60
excl-thin pairs with an identical pair set**):

| Commit | Move set | pane LOC after |
| --- | --- | --- |
| `483a73c` (5b1) | `draw_marquee` + `marquee_offset` + `draw_panel_at` + the `CAROUSEL_*` constants (+ the 2 marquee timing tests, 85 test LOC moved with their code, assertions untouched) → `widgets/marquee.rs` (282 LOC); lyrics **and** jellyfin cross-pane `ControlsPane::marquee_offset`/`draw_panel_at` calls flip to the widget | controls.rs 1516 → 1319 |
| `490c62e` (5b2) | `wrap_to_width` (pub(crate)) + `wrap_spans` (+ the wrap test moved with them) → `widgets/wrap.rs` (84 LOC); jellyfin's `lyrics::wrap_to_width` import flips to the widget | lyrics.rs 2543 → 2475 |
| `2fcb10c` (5b3) | button cluster — **documented decision NOT to merge** (§3, rationale below) | — |
| `ef4863f` (5b4) | now-playing line templates — **documented decision NOT to merge** (§3, rationale below) | — |
| 5b5 (close-out) | docs + metrics (this section, §2.4, HANDOFF, plan flipped done, session log) | — |

**Call sites (≥2 per extracted widget, the phase-5 exit criterion):**
`marquee.rs` is adopted by **controls** (2 render sites) + **lyrics**
(video-info title marquee) + **jellyfin** (header title marquee) — 3
panes; `wrap.rs` by **lyrics** (`wrap_to_width` ×2 + `wrap_spans`) +
**jellyfin** (episode overview) — 2 panes. The cross-pane coupling that
motivated the phase (`lyrics.rs`/`jellyfin.rs` reaching into
`ControlsPane`, `jellyfin.rs` reaching into `lyrics.rs`) is gone; panes
now reach into `ui::widgets` only.

**Real numbers (measured on committed refs with `ui-metrics.py --ref`
`64be282` vs `--ref HEAD` — the script reads the file list from the ref,
so new files are only counted once committed):**

- controls.rs **1516 → 1319 (−197)**: production 1038 → 926 (**−112**),
  tests 478 → 393 (−85, the 2 marquee timing tests moved with the code —
  assertions byte-identical, call paths updated to the widget).
- lyrics.rs **2543 → 2475 (−68)**: production 1424 → 1363 (**−61**),
  tests 1119 → 1112 (−7, the wrap test moved with its code).
- New widgets: `marquee.rs` **282** (193 prod + 89 test),
  `wrap.rs` **84** (72 prod + 12 test). `widgets/mod.rs` 21 → 23.
- Net LOC: **src/ui +103** (57,070 → 57,173), tree +103 (96,128 →
  96,231) — a pure move plus the widget headers/imports/doc-comment cost
  (marquee.rs carries the full cycle documentation with the constants);
  the marquee cycle is now one implementation shared by three panes, and
  the wrap helpers one implementation shared by two panes (the user
  priority: extensibility + predictable behavior; LOC is a proxy, not a
  gate).
- Similarity guardrail: **60 excl-thin pairs before and after**,
  identical pair set — the moved functions keep unique names and live in
  `ui/widgets/` (outside the pane-pair analysis), so no new >0.5 pairs
  appear and the controls/lyrics `draw_spans`/`draw_line` pairs (if any)
  are unaffected because those helpers stay in controls.rs per the plan.

**§3 decisions (documented in the plan §3a, both NOT to merge):**

1. **Button cluster** (`2fcb10c`): the lyrics header cluster (`LyricsBtn` +
   `button_line`), the controls mpv row-0 cluster (`mpv_button_layout`)
   and the transport row (`transport_zones`) are **three different
   shapes** — glyph+label spans with a space-padded ` | ` separator,
   2-tier width collapse, label-text-only hover (the `●`/space keep the
   base style, pinned cell-by-cell by `hover_highlights_only_the_label_text`)
   and a per-button `●`/`⭘` pressed marker vs plain whole labels with a
   1-col gap, no collapse (the title region shrinks instead), whole-label
   hover and `(x1,x2)` click zones vs a fixed 25-col centered slot row
   with literal pipes. Any single hover mode changes one side's visible
   behavior (whole-label hover would highlight the lyrics `●`; label-only
   hover would drop the mpv hover entirely — `⤓`/`[Audio]` contain no
   space to split). A shared widget would need a hover-scope enum, a
   separator enum, a collapse-tier table, an optional pressed-glyph
   callback and a zone-output shape — a fork, not an arg.
2. **Now-playing line templates** (`ef4863f`): controls
   `artist_title_line`/`channel_line` and the lyrics info header share
   only the *semantics* (what is playing). The data resolution differs
   (mpv/yt/mpd tag strategy + fallbacks vs Jellyfin-item/year-prefix/
   episode + yt-stream parts), the styles differ (theme-derived
   blur-following palette vs explicit ANSI white + yellow keys + bold
   `Time:`), and the layout differs (one centered marquee line vs fixed
   prefix + marquee window + context rows + description + pinned
   credits). The one genuinely shared piece — the marquee cycle — is
   already the 5b1 widget, adopted by all three panes.
3. **`ScrollingLine` (property.rs) kept** (plan §4a adopt-vs-keep): its
   cycle is a different shape — a continuous `|`-separated repeat
   (`line + " | " + line`, `(elapsed_sec × speed) % (line_len + 3)`),
   whole-second progress, no holds, no 3× faster wrap — vs the carousel's
   hold-2s → scroll-to-tail → hold-2s → 3× wrap with a 5-col gap. Unifying
   would change the property pane's scrolling behavior (modulo cycle vs
   phase cycle) — a behavior-visible change; `ScrollingLine` stays a thin
   widget (single caller by design) and the close-out records why.

**Remaining:** the phase-5 host live-check (plan §8 — controls carousel
cycle, lyrics header cluster + info marquee + wrap, jellyfin overview
wrap, property scrolling line); then Phase 6 (args expansion).

### 5.5 Phase 6 — args expansion close-out ✅ (2026-08-11)

Phase 6 moved the pane-specific browser constants into `PaneType`/config
args per the handoff plan `docs/design/Rewrite/phase6-args-expansion.md`,
in four commits on `rewrite` (each independently green, **1337/1337** at
the close — 1328 baseline + 9 new tests — warnings 3 baseline unchanged,
similarity guardrail **60 excl-thin pairs with an identical pair set**
before/after; `tree_args` was added to the script's thin-adapter list, the
same category as the Phase-1/2 hook accessors):

| Commit | Work | 
| --- | --- |
| `4a5b054` (6.1) | `TreeBrowserArgs` (serde + explicit `Default`: 50 / 120 / Some(15)) + the four variant fields on both `PaneTypeFile` and `PaneType`; manual `Deserialize` for `PaneTypeFile` (see below); the TryFrom conversion + every exhaustive match the compiler flagged (`{ .. }`); tests: bare-`Directories` parse with default args, explicit-args round-trip, defaults = today's constants |
| `a1caf6b` (6.2) | the four browser panes + `TreeBrowserCore` read the args: `TreeBrowserArgs::tree_width`/`info_box_height` (defaults = today's `tree_width()` / `.min(15)`), `Config::tree_browser_args` (first occurrence), a `tree_args` hook on `TreeBrowserCore` driving the shared `split_tree`, per-pane `layout_vertical`/render; the tree-width + info-cap parity tests stay green UNCHANGED; one new test per pane (directories + playlists + jellyfin: non-default args change the layout; radio: args plumb in, the 30% regions tree keeps its shape) |
| `9abb201` (6.3) | construction pattern: **documented decision** — recipe `docs/design/Rewrite/new-browser-tab.md` (config block + thin adapter over `TreeBrowserCore`/`BrowserPane`, never a new core) + a config→args bridge test; see the decision below |
| 6.4 (close-out) | docs + metrics (this section, §2.4, HANDOFF, plan flipped done, session log) |

**Backward compatibility (the load-bearing requirement).** A config.ron in
today's syntax — bare `Directories` / `Playlists` / `Jellyfin` / `Radio`
— parses with the default args; the round-23 config needs no edits. The
plan's §3 assumption that `#[serde(default)]` alone gives this "for free"
is **not** true for RON: a struct variant cannot be deserialized from its
unit form, so `PaneTypeFile`'s `Deserialize` is manual — it captures the
value with serde's Content (`serde::__private228`, the versioned hidden
module the derive itself uses; pinned by the lockfile), dispatches bare
unit names to the variant with default args, rewrites the captured
`{Variant: ()}` zero-tuple shape into the replay form `{Variant: Seq([])}`
(required because RON's value capture encodes every parenthesized variant
as a map, while serde's tuple-variant replay needs a sequence), and
replays the rest into a derived mirror enum (`PaneTypeFileArgs`, the same
shapes with `#[serde(default)] tree`) before converting. Pinned by
`bare_browser_panes_parse_with_default_tree_args` (all four panes) +
`explicit_tree_args_round_trip` + `default_args_are_today_s_constants`;
the example config/theme (with `Pane(Radio)`, `Pane(Empty())`,
`Property(Status(InputBuffer()))` …) parse unchanged.

**§6.3 decision (Phase-5 §3 rule — do NOT unify the four adapters).** The
outline's exit criterion — "adding a new browser tab = config block +
adapter, no new pane file" — is met by construction: the four browser
panes are already thin adapters over `TreeBrowserCore` (directories /
jellyfin / radio) and `BrowserPane`+`SongListCore` (playlists); a new
backend implements the ~15 tree hooks / 4 browser hooks + data fetch, adds
a `PaneType` block with `tree: TreeBrowserArgs`, and the shared core
provides render/mouse/actions. The recipe is `docs/design/Rewrite/
new-browser-tab.md` (plus a config→args bridge test
`config_tree_browser_args_read_the_first_occurrence_args`). Unifying the
four panes into ONE config-driven backend enum was attempted and rejected:
it would change observable behavior in four pinned places — radio's
focus-aware back-out + always-visible 30% regions tree (its own
`split_tree`, never collapsed), jellyfin's shared tree/items selection +
poster/info overlay + season expansion, playlists' list-shaped left pane
(`BrowserPane`, ♪/▶ kind prefixes — not a collapsible tree), and
directories' disk-backed never-cached Downloads folder. A backend enum
would need per-backend special cases for all four — a fork, not an arg
(the §2.3 root cause this rewrite exists to end). The cores stay one
implementation per shape; the panes stay thin per-backend adapters.

**Real numbers (ref-to-ref, `ui-metrics.py --ref 5a2ce9d` vs `--ref HEAD`
— the script's default reads the file list from HEAD and contents from
the worktree, so both sides must be committed refs):**

- `src/config/tabs.rs` 1277 → 1672 (+395: `TreeBrowserArgs` + the manual
  `Deserialize` machinery + the mirror enum + 4 tests); `config/mod.rs`
  1050 → 1076 (+26: `Config::tree_browser_args`).
- `src/ui/tree_browser.rs` 602 → 610 (+8: the `tree_args` hook);
  `directories.rs` 2121 → 2155, `jellyfin.rs` 2703 → 2736,
  `radio.rs` 2431 → 2467 (args field + constructor lookup + hook override
  + per-pane tests), `playlists.rs` 1755 → 1759 (+ render reads the args),
  `playlists/tests.rs` 1524 → 1554 (+2 layout tests).
- Match-churn files (panes/mod.rs, tab_help.rs, ui/mod.rs, paste.rs,
  event_loop.rs, work.rs) are line-count-neutral (pure `{ .. }` / args
  construction churn).
- Net LOC: **src/ui +145** (57,173 → 57,318), tree total .rs **+566**
  (96,231 → 96,797) — the phase front-loads the args/plumbing side (the
  serde machinery alone is +300 LOC of the tabs.rs delta; the
  backward-compat mechanism is the price of the load-bearing guarantee).
  Consumer paydown is the phase-7 close-out comparison.
- Tests **1328 → 1337** (+9: 3 config/args pins in 6.1, 5 per-pane layout
  tests in 6.2, 1 config→args bridge in 6.3); the tree-width +
  info-box-cap parity tests are byte-unchanged.
- Similarity guardrail **60 excl-thin before and after** (thin-adapter
  count 140 → 143 — the three `tree_args` overrides are thin accessors).

**Remaining:** the phase-6 host live-check (plan §8 — the four browser
tabs at ~70 vs wide widths, info boxes ≈ 15 rows, a config override
`tree_min_width: 60` / `info_box_cap: None` followed by a restart, and the
round-23 config needing NO edits); then Phase 7 (close-out).

### 5.6 Phase 7 — rewrite close-out ✅ (2026-08-11)

Phase 7 closed the rewrite: FINAL metrics, the docs/design `source_files`
sweep, HANDOFF/notes final state, the branch-state writeup and the session
log — docs/metrics only, **zero code edits**. Three commits on `rewrite`,
each independently green (**1337/1337**, warnings 3 baseline unchanged,
similarity guardrail **60 excl-thin** after every commit):

| Commit | Contents |
| --- | --- |
| 7.1 (docs) | FINAL LOC comparison `24bd883` vs `HEAD`; §2.4 table → final; this §5.6 per-phase table |
| 7.2 (docs) | docs/design `source_files`/`related` sweep (stale paths fixed, `updated:` bumped); HANDOFF rewrite section → final; notes.md rewrite-complete block; `docs/design/Rewrite/REVIEW.md` (new, branch-state review) |
| 7.3 (docs) | session log Phase 7 entry (`docs/design/Sessions/2026-08-11.md`); final self-review (sweep grep + full gate) |

**The rewrite is LOC-positive overall — reported plainly, no spin.** From
the Phase-0 baseline `24bd883` to `HEAD` the tree is **+986 `.rs` LOC**
(src/ui **+614**): the master-module pattern front-loads the shared cores
(`song_list.rs` +955, `tree_browser.rs` +610, `list_modal.rs` +765,
`marquee.rs` +282, `sub_tab_bar.rs` +149, the Phase-6 serde machinery
+395 in `tabs.rs`) while the consumers shrank (browser.rs 1041 → 232,
queue.rs 4686 → 3485, controls.rs 1516 → 1319, search −236, select/torrent/
decoders −227/−360/−181) — and the user priority is extensibility +
predictable behavior: one implementation per shape, bought at the price of
args/plumbing LOC; **LOC is a proxy, not a gate** (§1). The per-phase
ref-to-ref numbers (each phase's boundary commit, `ui-metrics.py --ref`):

| Phase | Boundary ref | src/ui | Δ src/ui | tree .rs | Δ tree | cumulative Δ vs baseline (src/ui / tree) |
| --- | --- | --- | --- | --- | --- | --- |
| 0 — baseline | `24bd883` | 56,704 | — | 95,811 | — | — |
| 1 — SongListCore | `6af6f07` | 56,525 | −179 | 95,632 | −179 | −179 / −179 |
| 2 — TreeBrowserCore | `eb401ce` | 56,755 | +230 | 95,862 | +230 | +51 / +51 |
| 2.1 — delta close-out | `3e667ed` | 56,923 | +168 | 96,030 | +168 | +219 / +219 |
| 3 — modal masters | `15f9707` | 56,868 | −55 | 95,926 | −104 | +164 / +115 |
| 4a — SubTabBar | `9b46f54` | 56,987 | +119 | 96,045 | +119 | +283 / +234 |
| 4b — queue split | `9fee28e` | 57,070 | +83 | 96,128 | +83 | +366 / +317 |
| 5 — drawing widgets | `671bc91` | 57,173 | +103 | 96,231 | +103 | +469 / +420 |
| 6 — args expansion | `1e2aa1d` | 57,318 | +145 | 96,797 | +566 | +614 / +986 |
| 7 — close-out (docs) | `HEAD` | 57,318 | +0 | 96,797 | +0 | +614 / +986 |

> Phase deltas are measured on the phase's last commit (the boundary refs
> above). Two notes on consistency with the per-phase close-outs: Phase 3's
> close-out text says −61/−110 — that was measured at `53b9e90`; the fmt
> wrap commit `48004f1` (+6) landed inside the phase, so the FINAL Phase-3
> delta is −55/−104 at `15f9707`. Phase 4a's +119 is `sub_tab_bar.rs` (149
> incl. 2 tests) minus the queue toggle extraction; it was not recorded in
> the 4a close-out, this table is the authoritative number. Everything else
> matches the per-phase close-outs exactly.

**Guardrail (final):** 42 excl-thin pairs > 0.5 at baseline `24bd883` and
**60 at HEAD** with the current thin-adapter list (the historical 51-at-
baseline figure used the Phase-1-era list; see §2.4). **Tests:** 1312 at
baseline → **1337/1337 at HEAD** (warnings 3 baseline unchanged).
**Docs:** HANDOFF rewrite section is FINAL (next → none); `notes.md` has
the rewrite-complete block; `REVIEW.md` is the review entry point (branch
state, review recipe, remaining host live-checks, known caveats).

**Remaining:** the host live-checks from phases 4b/5/6 (REVIEW.md lists
them), then the host pushes `rewrite` (user rule: the agent never pushes).
The rewrite itself is complete: `master` untouched, test count unchanged.

## 6. Definition of done (applies per phase)


1. `cargo test --release` green (1312 and growing), no new warnings.
2. Behavior parity: the tabs/panes/modals touched by the phase pass the
   existing design-doc specs and a live check by the user.
3. No two panes implement the same-named function with similarity > 0.5
   (the clone pairs in §2.2 are gone or reduced to shared call sites).
4. Net LOC reduction for the phase (tracked against the Phase-0 table)
   — secondary to the user priority (extensibility + predictable
   behavior, §1); a phase may close LOC-neutral/positive when the
   shared-core cost is the price of one-implementation-by-construction
   (record the real ref-to-ref number in the close-out either way).
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
