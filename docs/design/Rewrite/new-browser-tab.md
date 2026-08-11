---
title: "Adding a new browser tab (construction pattern)"
section: rewrite
doc_type: recipe
id: "rewrite/new-browser-tab"
description: >
  Phase-6 construction pattern for new browser tabs: a config block
  (a PaneType variant + tree args) plus a thin adapter over the shared
  TreeBrowserCore / BrowserPane cores — never a new core implementation.
  Also records why the four existing browser panes stay per-backend
  adapters (Phase-5 §3 decision rule).
status: "current"
updated: "2026-08-11"
related:
  - rewrite/ui-reuse
  - rewrite/phase6
tags: [rewrite, panes, architecture, recipe]
---

# Adding a new browser tab (construction pattern)

The Phase-6 target architecture (ui-reuse-rewrite.md §3) says a new tab is
*config + args*, not a copy of a pane file. For the browser family this
means: **a new browser tab = a config block (a `PaneType` variant carrying
`tree: TreeBrowserArgs`) + a thin adapter over the shared cores — never a
new core implementation.**

## 1. The pattern

The browser family is two shapes, each with one master implementation:

| Shape | Master core | Adapters (today) |
| --- | --- | --- |
| tree + items (left tree, right list, tips, info box, temp-play) | `TreeBrowserCore` (`src/ui/tree_browser.rs`) | `DirectoriesPane`, `JellyfinPane`, `RadioPane` |
| list + info (left list, right songs, info box) | `BrowserPane` + `SongListCore` (`src/ui/browser.rs`, `src/ui/song_list.rs`) | `PlaylistsPane`, `TagBrowserPane`, `AlbumsPane` |

Every pane-specific behavior — tree row content, item rows, context menus,
data fetching, the info box, temp-play cleanup — is a **hook** on the core
(the Phase-1/2 trait-with-hooks pattern). A new backend implements the
hooks + data-fetch glue; the shared mechanics (render, mouse, marking,
scroll, navigation, actions) come from the core unchanged.

## 2. The recipe (adding a browser tab)

1. **Config block**: add a `PaneType`/`PaneTypeFile` variant (or reuse one)
   carrying `#[serde(default)] tree: TreeBrowserArgs` — bare
   `Directories`-style syntax keeps parsing with the round-23 defaults
   (50 / 120 / Some(15)).
2. **Adapter**: implement the core's hooks for the new source (the tree
   rows + items + fetch + info box; `TreeBrowserCore` needs ~15 small
   hooks, `BrowserPane` four). Thin data glue only — no render/mouse/
   action reimplementation.
3. **Wire the args**: read `ctx.config.tree_browser_args(<discriminant>)`
   in the adapter's constructor (the first config occurrence drives the
   singleton pane — `Config::tree_browser_args`), or thread the args
   through `PaneContainer::new` for per-instance panes.
4. **Add the tab** in `~/.config/s2udio/config.ron` (a `Pane(X)` block
   inside a `Split`); the pane appears with its layout args.

The proof it works today: `PaneType::Browser { root_tag, separator }` +
`TagBrowserPane` is a config-only list-browser tab (no pane file), and the
four tree-browser panes share `TreeBrowserCore`'s render/mouse/actions
wholesale — the phase-2 consolidation that deleted the three parallel
implementations.

## 3. Why the four adapters stay per-backend (Phase-5 §3 decision rule)

Unifying `DirectoriesPane` / `PlaylistsPane` / `JellyfinPane` /
`RadioPane` into ONE config-driven backend enum would change observable
behavior in four pinned places — a fork, not an arg:

- **Radio focus handling**: a single keyboard cursor moves between regions
  and stations with focus-aware back-out, the regions tree is always
  visible (its own `split_tree`: 30% share, no narrow-TUI collapse), and
  the station cursor never moves the tree highlight. The other three panes
  sync the tree to the items cursor.
- **Jellyfin shared-selection**: the poster/metadata info box is a shared
  selection across the tree and items (a terminal-side image overlay with
  its own lifecycle), plus season expansion opens leaf containers on
  double-click.
- **Playlists song pane**: the left pane is a *list* (stored playlists,
  ♪/▶ kind prefixes), not a collapsible tree — PlaylistsPane implements
  `BrowserPane`/`SongListCore`, not `TreeBrowserCore`.
- **Directories Downloads**: the Downloads folder is listed from disk (not
  MPD), never cached, and pinned at the root.

These are the hooks already parameterized by the cores. A single
backend-enum pane would need per-backend special cases for all four —
exactly the fork the Phase-5 §3 rule (and the §2.3 root-cause analysis)
rejects. The cores stay one-implementation-per-shape; the panes stay thin
per-backend adapters; new browser tabs are config blocks + new adapters
over the same cores.

## 4. Args recap (phase 6)

`TreeBrowserArgs { tree_min_width, tree_hide_below, info_box_cap }` with
serde defaults 50 / 120 / Some(15) — today's constants. The panes read
them via `Config::tree_browser_args` (first occurrence) and the shared
`TreeBrowserCore::split_tree` / the panes' `layout_vertical` honor them;
`info_box_cap: None` removes the 15-row info cap (round-8 behavior).
